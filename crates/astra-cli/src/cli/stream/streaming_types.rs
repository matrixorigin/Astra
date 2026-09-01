//! SSE Streaming Types
//!
//! Data structures for streaming chat responses and handling turn failures.
//! These types bridge the agentic runtime with the CLI display logic.

/// Re-export of the verdict audit event type for convenience.
pub(crate) type VerdictEvent = astra_turn_core::guardrails::verdict_audit::AgenticVerdictAuditEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppliedStreamUserIntent {
    pub(crate) intent_id: String,
    pub(crate) delivery: astra_turn_types::UserIntentDelivery,
    pub(crate) status: astra_turn_types::UserIntentStatus,
    pub(crate) event_index: usize,
    pub(crate) content: String,
}

/// Failure of the CLI's public stdout transport, kept distinct from model,
/// tool, and Server failures so the process can finish exact remote cleanup
/// before applying the conventional pipeline exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputTransportFailure {
    Closed,
    Failed,
}

impl OutputTransportFailure {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Closed => "stdout output transport closed by its consumer",
            Self::Failed => "stdout output transport failed",
        }
    }
}

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
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub tool_calls_count: u32,
    /// Logical provider rounds observed before failure. Unlike retained tool
    /// records, this survives stream cancellation and record-window eviction.
    pub llm_rounds: Option<u32>,
    /// Provider usage coverage captured before failure.
    pub token_usage_coverage: astra_turn_core::chat_turn_sse_dispatch::TokenUsageCoverage,
    /// Durable run-total tool lifecycle accounting. This remains separate
    /// from local records because a server-owned executor may have requested,
    /// rejected, or completed calls the thin client never observed live.
    pub tool_outcomes: Option<astra_services::session_journal::ToolOutcomeSummary>,
    /// Durable guidance applied by a server-owned loop before this attempt
    /// failed. Failure is not permission to erase user-authored input from
    /// local restart history.
    pub applied_user_intents: Vec<AppliedStreamUserIntent>,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub last_heavy_checkpoint: Option<astra_pipeline::step_protocol::StepCheckpoint>,
    /// Partial text the model generated before the turn was interrupted.
    /// Preserved in conversation history so the next turn has context.
    pub partial_text: String,
    /// Exact prompt-history items captured before the failed runtime loop
    /// returned. This survives independently of `partial_text`, which is only
    /// a user-facing suffix and cannot represent tools or reasoning.
    pub run_transcript_messages: Vec<serde_json::Value>,
    /// The local client can no longer settle or observe an admitted durable
    /// run, so that exact owner must receive an explicit cancellation request.
    pub remote_cancel_required: bool,
    /// Exact durable owner: a callback-producer child when callback delivery
    /// failed, otherwise the immutable physical SSE root.
    pub remote_cancel_run_id: Option<String>,
    /// Typed public-output failure that caused this turn to stop. This is set
    /// only when output transport, rather than a model/tool failure, owns the
    /// terminal outcome.
    pub output_transport_failure: Option<OutputTransportFailure>,
    /// Structured interruption captured by the runtime before returning a
    /// failure.  Keeping this alongside the other partial facts lets one-shot
    /// JSON surfaces preserve a typed, resumable terminal instead of dropping
    /// the entire envelope when the loop returns `TurnFailure`.
    pub interruption: Option<serde_json::Value>,
}

pub(crate) fn unsettled_physical_owner_run_id(
    accum: &astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum,
) -> Option<String> {
    (accum.error_kind.is_some() && accum.run_terminal.is_none())
        .then(|| accum.run_id.clone())
        .flatten()
        .filter(|run_id| !run_id.trim().is_empty())
}

/// A turn failure that carries partial data for post-mortem analysis.
#[derive(Debug)]
pub(crate) struct TurnFailure {
    pub error: String,
    pub partial: PartialTurnData,
}

/// Promote a typed, resumable runtime interruption into the same terminal
/// shape used by the normal stream path.
///
/// A `TurnFailure` is still a hard error unless the runtime supplied a known
/// interruption record marked resumable.  In particular, this deliberately
/// refuses to infer lifecycle state from free-form provider error text, so
/// authentication, harness, and unknown failures remain fail-closed.
pub(crate) fn stream_result_from_resumable_turn_failure(
    failure: &TurnFailure,
) -> Option<StreamResult> {
    let interruption = failure.partial.interruption.as_ref()?;
    let kind_label = interruption.get("kind")?.as_str()?;
    let kind = astra_turn_core::interruption::InterruptionKind::from_label(kind_label)?;
    if !kind.is_resumable()
        || interruption
            .get("resumable")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
    {
        return None;
    }

    let mut full_text = failure.partial.partial_text.clone();
    if full_text.trim().is_empty() {
        full_text = interruption
            .get("user_message")
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(ToString::to_string)
            .unwrap_or_else(|| {
                format!("The turn was interrupted ({kind_label}) and can be continued.")
            });
    }

    Some(StreamResult {
        session_id: failure.partial.session_id.clone(),
        run_id: failure.partial.run_id.clone(),
        full_text,
        prompt_tokens: failure.partial.prompt_tokens,
        completion_tokens: failure.partial.completion_tokens,
        cache_read_tokens: failure.partial.cache_read_tokens,
        cache_creation_tokens: failure.partial.cache_creation_tokens,
        tool_calls_count: failure.partial.tool_calls_count,
        llm_rounds: failure.partial.llm_rounds,
        token_usage_coverage: failure.partial.token_usage_coverage,
        tools_used: failure.partial.tools_used.clone(),
        tool_call_records: failure.partial.tool_call_records.clone(),
        stall_events: failure.partial.stall_events.clone(),
        verdict_events: failure.partial.verdict_events.clone(),
        last_heavy_checkpoint: failure.partial.last_heavy_checkpoint.clone(),
        interruption: Some(interruption.clone()),
        final_state: "interrupted".to_string(),
        interruption_kind: Some(kind.label().to_string()),
        // A failed loop has not produced a verified terminal envelope.  Keep
        // this fact explicit even when the interruption is resumable.
        server_terminal_unverified: true,
        tool_record_coverage_partial: true,
        run_transcript_messages: failure.partial.run_transcript_messages.clone(),
        ..StreamResult::default()
    })
}

impl std::fmt::Display for TurnFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}

impl std::error::Error for TurnFailure {}

pub(crate) fn apply_partial_turn_data_to_error_event(
    event: &mut astra_services::session_journal::JournalEvent,
    partial: &PartialTurnData,
) {
    *event = event.clone().with_run_id(partial.run_id.as_deref());
    if !partial.tool_call_records.is_empty() {
        event.tool_calls = Some(partial.tool_call_records.clone());
    }
    if partial.prompt_tokens > 0 {
        event.tokens_in = Some(partial.prompt_tokens);
    }
    if partial.completion_tokens > 0 {
        event.tokens_out = Some(partial.completion_tokens);
    }
    if partial.cache_read_tokens > 0 {
        event.cache_read_tokens = Some(partial.cache_read_tokens);
    }
    if partial.cache_creation_tokens > 0 {
        event.cache_creation_tokens = Some(partial.cache_creation_tokens);
    }
    if partial.tool_calls_count > 0 {
        event.tool_count = Some(partial.tool_calls_count);
    }
    if !partial.tools_used.is_empty() {
        event.tools_used = Some(partial.tools_used.clone());
    }
}

/// Convert root append-boundary capture into the local durable transcript
/// contract used by child runs. `run_id + item_seq`, not content, is the
/// identity used by readers to make an ambiguous append retry idempotent.
pub(crate) fn root_run_transcript_events(
    session_id: Option<&str>,
    run_id: Option<&str>,
    messages: &[serde_json::Value],
) -> Vec<astra_services::session_journal::JournalEvent> {
    let (Some(session_id), Some(run_id)) = (session_id, run_id) else {
        return Vec::new();
    };
    if session_id.trim().is_empty() || run_id.trim().is_empty() {
        return Vec::new();
    }

    messages
        .iter()
        .filter(|message| {
            !matches!(
                message.get("role").and_then(serde_json::Value::as_str),
                Some("system") | None
            )
        })
        .enumerate()
        .filter_map(|(index, message)| {
            let item_seq = u64::try_from(index).ok()?.saturating_add(1);
            astra_services::session_journal::JournalEvent::transcript_item(
                session_id, run_id, "root", item_seq, message,
            )
        })
        .collect()
}

/// Result of a streaming chat turn, including token counts and tool usage data.
#[derive(Debug)]
pub(crate) struct StreamResult {
    pub(crate) session_id: Option<String>,
    pub(crate) run_id: Option<String>,
    /// Durable local persistence failure recorded after the runtime finished
    /// successfully (for example one-shot journal append failure).
    pub(crate) session_persistence_error: Option<String>,
    pub(crate) full_text: String,
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_creation_tokens: u64,
    pub(crate) tool_calls_count: u32,
    /// Canonical fixed-size closure of every local and remote tool attempt in
    /// this logical turn. Unlike `tool_call_records`, this is never a trimmed
    /// audit window and therefore owns terminal result-class projection.
    pub(crate) tool_ledger_aggregate:
        astra_turn_core::tool_ledger_receipt::ToolLedgerCanonicalAggregate,
    /// Tool names visible to the LLM (first turn surface report).
    pub(crate) visible_tools: Vec<String>,
    /// Skill names selected by the LLM during tool surface.
    pub(crate) selected_skills: Vec<String>,
    /// Tool names with material execution across all turns.
    pub(crate) tools_used: Vec<String>,
    /// Deferred schemas materialized in the retained session context.
    pub(crate) activated_deferred_tool_names: Vec<String>,
    /// Per-tool-call audit records: name, ok, ms, error.
    pub(crate) tool_call_records: Vec<astra_services::session_journal::ToolCallRecord>,
    /// Token budget used by selected dynamic tools.
    pub(crate) budget_used: u32,
    /// Token budget pressure (0.0-0.9) from compaction tier.
    pub(crate) budget_pressure: f64,
    /// Stall events that occurred during the agentic loop (stall_type, turn_number).
    pub(crate) stall_events: Vec<(String, u32)>,
    /// TurnGuard verdict events (severity, turn, injections, avoid_tools,
    /// advisory threshold, error pressure, and health telemetry). Only
    /// non-Healthy verdicts.
    pub(crate) verdict_events: Vec<VerdictEvent>,
    /// Step Protocol recorder summary for debugging and audit.
    pub(crate) step_recorder_summary: Option<astra_pipeline::step_recorder::RecorderSummary>,
    /// Exported tool health entries from this turn's TurnGuard (for cross-session persistence).
    pub(crate) tool_health_export: Vec<astra_turn_core::tool_health_persistence::ToolHealthEntry>,
    /// Last heavy checkpoint built during the agentic loop (for cloud persistence).
    pub(crate) last_heavy_checkpoint: Option<astra_pipeline::step_protocol::StepCheckpoint>,
    /// Time to first token in milliseconds.
    pub(crate) ttft_ms: Option<u64>,
    /// Context assembly time in milliseconds.
    pub(crate) context_ms: Option<u64>,
    /// Memoria search time in milliseconds (subset of context_ms).
    pub(crate) memoria_ms: Option<u64>,
    /// LLM-judged routing domain for this user line. `None` means the strict
    /// turn-intent judge was unavailable or returned no reliable domain.
    pub(crate) routing_domain_hint: Option<String>,
    /// Entity graph skipped learning: success with tools but no routing domain.
    pub(crate) entity_learn_skipped_no_domain: bool,
    /// Deferred context assembly trace: journal event is only written on turn commit.
    pub(crate) pending_context_assembly_trace: Option<(u32, serde_json::Value)>,
    /// Collected turn observability events (llm_round, tool timing) for batch flush.
    pub(crate) turn_observability_events: Vec<astra_services::session_journal::JournalEvent>,
    /// Aggregated LLM round count for this turn.
    pub(crate) llm_rounds: Option<u32>,
    /// Provider-reported token usage coverage. Token totals are lower bounds
    /// unless this reports `complete`.
    pub(crate) token_usage_coverage: astra_turn_core::chat_turn_sse_dispatch::TokenUsageCoverage,
    /// Structured interruption context when the runtime completed the turn
    /// partially (for example due to budget exhaustion after tool progress).
    pub(crate) interruption: Option<serde_json::Value>,
    /// Machine-readable terminal state: completed, interrupted, or empty.
    pub(crate) final_state: String,
    /// Interruption kind label when final_state is interrupted.
    pub(crate) interruption_kind: Option<String>,
    /// The authoritative Server-owned loop reported unresolved/rejected
    /// execution evidence.  The local CLI may have no per-call records in
    /// this topology, so terminal disposition must retain this fact instead
    /// of silently presenting the response as verified.
    pub(crate) server_terminal_unverified: bool,
    /// Whether a Server-owned terminal is the final outcome authority for
    /// this logical turn. Local edge records remain audit evidence but cannot
    /// override this typed terminal fact.
    pub(crate) server_terminal_authoritative: bool,
    /// Whether any remote server run contributed tool calls without exposing
    /// per-call records to this edge process. This coverage fact is
    /// independent from the final terminal authority: a later edge terminal
    /// may own exit status while the aggregate record view remains partial.
    pub(crate) tool_record_coverage_partial: bool,
    /// Full messages array after this turn — used by CslManager for persistence.
    pub(crate) final_messages: Vec<serde_json::Value>,
    /// Exact prompt-history items appended by this root execution run.
    ///
    /// This is captured at the runtime append boundary before compaction may
    /// rewrite `final_messages`. It is the local root counterpart to a child
    /// run's canonical transcript capture, not a prompt reconstruction hint.
    pub(crate) run_transcript_messages: Vec<serde_json::Value>,
    /// Structured user intents applied while the turn was already active.
    /// This is the durable fact source for active-run guidance; `final_messages`
    /// is only a prompt projection fallback.
    pub(crate) applied_user_intents: Vec<AppliedStreamUserIntent>,
    /// Results from background-spawned agents collected after the agentic
    /// loop ended. Each entry is (agent_id, result_text).
    pub(crate) background_agent_results: Vec<(String, String)>,
}

impl StreamResult {
    /// Merge terminal background-agent outputs into the user-facing aggregate
    /// response used by one-shot CLI and server surfaces.
    ///
    /// Interactive turns reconcile the same facts through the root mailbox on
    /// a later model step. One-shot surfaces have no later step, so leaving the
    /// drain results only in an internal field would make completed work
    /// invisible to text consumers.
    pub(crate) fn integrate_background_agent_results(&mut self) -> Option<String> {
        let section = format_background_agent_results(&self.background_agent_results)?;
        if !self.full_text.is_empty() {
            self.full_text.push_str("\n\n");
        }
        self.full_text.push_str(&section);
        Some(section)
    }

    /// User input that should represent this committed turn in durable history.
    ///
    /// The runtime can apply user guidance while a turn is executing.
    /// Those messages are already appended to `final_messages` as prompt-facing
    /// user messages, so durable history must derive from final prompt history
    /// instead of only the original line submitted at turn start.
    pub(crate) fn effective_user_input(&self, primary_line: &str) -> String {
        if !self.applied_user_intents.is_empty() {
            return effective_user_input_from_applied_user_intents(
                primary_line,
                &self.applied_user_intents,
            );
        }
        effective_user_input_from_messages(primary_line, &self.final_messages)
    }

    /// Latest user instruction that should drive follow-up suggestions,
    /// relevance checks, and continuation anchors.
    pub(crate) fn latest_user_input(&self, primary_line: &str) -> String {
        if let Some(input) = self.applied_user_intents.last() {
            return input.content.clone();
        }
        latest_user_input_from_messages(primary_line, &self.final_messages)
    }
}

pub(crate) fn format_background_agent_results(results: &[(String, String)]) -> Option<String> {
    if results.is_empty() {
        return None;
    }

    let mut section = String::from("## Background agent results");
    for (agent_id, result) in results {
        section.push_str("\n\n### Agent `");
        section.push_str(agent_id);
        section.push_str("`\n\n");
        section.push_str(result.trim());
    }
    Some(section)
}

fn effective_user_input_from_applied_user_intents(
    primary_line: &str,
    inputs: &[AppliedStreamUserIntent],
) -> String {
    let primary = primary_line.trim();
    let mut parts = Vec::new();
    if !primary.is_empty() {
        parts.push(primary.to_string());
    }
    parts.extend(
        inputs
            .iter()
            .map(|input| input.content.trim())
            .filter(|content| !content.is_empty())
            .map(ToString::to_string),
    );
    parts.join("\n\n")
}

fn effective_user_input_from_messages(
    primary_line: &str,
    messages: &[serde_json::Value],
) -> String {
    user_inputs_from_current_turn(primary_line, messages).join("\n\n")
}

fn latest_user_input_from_messages(primary_line: &str, messages: &[serde_json::Value]) -> String {
    user_inputs_from_current_turn(primary_line, messages)
        .last()
        .cloned()
        .unwrap_or_default()
}

fn user_inputs_from_current_turn(
    primary_line: &str,
    messages: &[serde_json::Value],
) -> Vec<String> {
    let primary = primary_line.trim();
    if primary.is_empty() {
        return Vec::new();
    }
    let measure_history_work = astra_core::history_work::instrumentation_enabled();
    let mut projected_bytes = 0_u64;
    let user_contents = messages
        .iter()
        .filter_map(|message| {
            let role = message.get("role")?.as_str()?;
            if role != "user" {
                return None;
            }
            let content = message.get("content")?.as_str()?.trim();
            (!content.is_empty()).then(|| {
                if measure_history_work {
                    projected_bytes = projected_bytes
                        .saturating_add(content.len().try_into().unwrap_or(u64::MAX));
                }
                content.to_string()
            })
        })
        .collect::<Vec<_>>();
    if measure_history_work {
        astra_core::history_work::record_operation(
            astra_core::history_work::HistoryWorkSite::CliTurnUserInputProjection,
            projected_bytes,
            messages.len().try_into().unwrap_or(u64::MAX),
            0,
        );
    }

    let Some(start) = user_contents
        .iter()
        .rposition(|content| content.trim() == primary)
    else {
        return vec![primary.to_string()];
    };

    user_contents[start..].to_vec()
}

#[cfg(test)]
mod user_input_tests {
    use super::{
        AppliedStreamUserIntent, effective_user_input_from_applied_user_intents,
        effective_user_input_from_messages, latest_user_input_from_messages,
    };
    use serde_json::json;

    #[test]
    fn effective_user_input_prefers_structured_deferred_events() {
        let inputs = vec![
            AppliedStreamUserIntent {
                intent_id: "intent-2".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::Applied,
                event_index: 4,
                content: "2".to_string(),
            },
            AppliedStreamUserIntent {
                intent_id: "intent-3".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::Applied,
                event_index: 5,
                content: "3".to_string(),
            },
        ];

        assert_eq!(
            effective_user_input_from_applied_user_intents("1", &inputs),
            "1\n\n2\n\n3"
        );
    }

    #[test]
    fn effective_user_input_includes_deferred_messages_after_primary_line() {
        let messages = vec![
            json!({"role": "user", "content": "old"}),
            json!({"role": "assistant", "content": "old answer"}),
            json!({"role": "user", "content": "1"}),
            json!({"role": "assistant", "content": "working"}),
            json!({"role": "user", "content": "2"}),
        ];

        assert_eq!(effective_user_input_from_messages("1", &messages), "1\n\n2");
        assert_eq!(latest_user_input_from_messages("1", &messages), "2");
    }

    #[test]
    fn effective_user_input_uses_last_matching_primary_line() {
        let messages = vec![
            json!({"role": "user", "content": "repeat"}),
            json!({"role": "assistant", "content": "old answer"}),
            json!({"role": "user", "content": "repeat"}),
            json!({"role": "user", "content": "deferred"}),
        ];

        assert_eq!(
            effective_user_input_from_messages("repeat", &messages),
            "repeat\n\ndeferred"
        );
    }

    #[test]
    fn effective_user_input_falls_back_to_primary_when_history_was_compacted() {
        let messages = vec![json!({"role": "assistant", "content": "summary"})];

        assert_eq!(
            effective_user_input_from_messages("current", &messages),
            "current"
        );
    }
}

impl Default for StreamResult {
    fn default() -> Self {
        Self {
            session_id: None,
            run_id: None,
            session_persistence_error: None,
            full_text: String::new(),
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_count: 0,
            tool_ledger_aggregate: Default::default(),
            visible_tools: vec![],
            selected_skills: vec![],
            tools_used: vec![],
            activated_deferred_tool_names: vec![],
            tool_call_records: vec![],
            budget_used: 0,
            budget_pressure: 0.0,
            stall_events: vec![],
            verdict_events: vec![],
            step_recorder_summary: None,
            tool_health_export: vec![],
            last_heavy_checkpoint: None,
            ttft_ms: None,
            context_ms: None,
            memoria_ms: None,
            routing_domain_hint: None,
            entity_learn_skipped_no_domain: false,
            pending_context_assembly_trace: None,
            turn_observability_events: Vec::new(),
            llm_rounds: None,
            token_usage_coverage: Default::default(),
            interruption: None,
            final_state: "completed".to_string(),
            interruption_kind: None,
            server_terminal_unverified: false,
            server_terminal_authoritative: false,
            tool_record_coverage_partial: false,
            final_messages: Vec::new(),
            run_transcript_messages: Vec::new(),
            applied_user_intents: Vec::new(),
            background_agent_results: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PartialTurnData, TurnFailure, apply_partial_turn_data_to_error_event,
        stream_result_from_resumable_turn_failure,
    };
    use astra_services::session_journal::{JournalEvent, ToolCallRecord};
    use serde_json::json;

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
            run_id: Some("run-123".into()),
            prompt_tokens: 42,
            completion_tokens: 21,
            cache_read_tokens: 9,
            cache_creation_tokens: 4,
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
        assert_eq!(event.cache_read_tokens, Some(9));
        assert_eq!(event.cache_creation_tokens, Some(4));
        assert_eq!(event.tool_calls.as_ref().map(Vec::len), Some(2));
        assert_eq!(event.metadata.as_ref().unwrap()["run_id"], "run-123");
    }

    #[test]
    fn resumable_turn_failure_becomes_typed_partial_result() {
        let failure = TurnFailure {
            error: "[server_error] [budget_exhausted] provider budget exhausted".into(),
            partial: PartialTurnData {
                session_id: Some("session-1".into()),
                run_id: Some("run-1".into()),
                prompt_tokens: 123,
                completion_tokens: 45,
                tool_calls_count: 2,
                partial_text: "partial answer".into(),
                interruption: Some(json!({
                    "kind": "budget_exhausted",
                    "resumable": true,
                    "user_message": "Continue to resume.",
                })),
                ..Default::default()
            },
        };

        let result = stream_result_from_resumable_turn_failure(&failure)
            .expect("typed resumable interruptions should be preserved");
        assert_eq!(result.final_state, "interrupted");
        assert_eq!(
            result.interruption_kind.as_deref(),
            Some("budget_exhausted")
        );
        assert_eq!(result.session_id.as_deref(), Some("session-1"));
        assert_eq!(result.run_id.as_deref(), Some("run-1"));
        assert_eq!(result.prompt_tokens, 123);
        assert_eq!(result.completion_tokens, 45);
        assert_eq!(result.tool_calls_count, 2);
        assert_eq!(result.full_text, "partial answer");
        assert!(result.server_terminal_unverified);
        assert!(result.tool_record_coverage_partial);
    }

    #[test]
    fn non_resumable_or_untyped_failure_stays_hard_error() {
        for interruption in [
            json!({"kind": "auth_failure", "resumable": false}),
            json!({"kind": "budget_exhausted", "resumable": false}),
            json!({"kind": "unknown_future_kind", "resumable": true}),
        ] {
            let failure = TurnFailure {
                error: "provider failure".into(),
                partial: PartialTurnData {
                    interruption: Some(interruption),
                    ..Default::default()
                },
            };
            assert!(
                stream_result_from_resumable_turn_failure(&failure).is_none(),
                "untrusted interruption must not be promoted"
            );
        }
    }
}
