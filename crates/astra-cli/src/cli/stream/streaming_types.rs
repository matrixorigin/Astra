//! SSE Streaming Types
//!
//! Data structures for streaming chat responses and handling turn failures.
//! These types bridge the agentic runtime with the CLI display logic.

use std::collections::BTreeSet;

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

/// Token lanes attributed to one execution class. `None` means that the
/// provider did not report that lane; it is deliberately different from zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AttributedTokenUsage {
    pub(crate) fresh_input_tokens: Option<u64>,
    pub(crate) cache_read_tokens: Option<u64>,
    pub(crate) cache_creation_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
}

impl AttributedTokenUsage {
    pub(crate) fn known_total_tokens(self) -> u64 {
        self.fresh_input_tokens
            .unwrap_or(0)
            .saturating_add(self.cache_read_tokens.unwrap_or(0))
            .saturating_add(self.cache_creation_tokens.unwrap_or(0))
            .saturating_add(self.output_tokens.unwrap_or(0))
    }
}

#[derive(Debug, Default)]
struct UsageLaneAccumulator {
    usage: AttributedTokenUsage,
    observed: bool,
}

impl UsageLaneAccumulator {
    fn add(&mut self, usage: &astra_turn_types::ExplainAnalyzeTokenUsageV1) {
        self.observed = true;
        fn add_lane(total: &mut Option<u64>, value: Option<u64>) {
            if let Some(value) = value {
                *total = Some(total.unwrap_or_default().saturating_add(value));
            }
        }
        add_lane(&mut self.usage.fresh_input_tokens, usage.fresh_input_tokens);
        add_lane(&mut self.usage.cache_read_tokens, usage.cache_read_tokens);
        add_lane(
            &mut self.usage.cache_creation_tokens,
            usage.cache_creation_tokens,
        );
        add_lane(&mut self.usage.output_tokens, usage.output_tokens);
    }

    fn finish(self) -> Option<AttributedTokenUsage> {
        self.observed.then_some(self.usage)
    }
}

/// One logical turn's usage attribution. Overall totals remain available on
/// `StreamResult` for billing/session accounting, while this split is the only
/// source allowed for the compact user-facing per-turn summary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UsageAttribution {
    pub(crate) primary: Option<AttributedTokenUsage>,
    pub(crate) primary_complete: bool,
    /// Input-only eligibility, derived from canonical facts; never persisted.
    pub(crate) primary_input_complete: bool,
    pub(crate) primary_attempts: u32,
    pub(crate) primary_model: Option<String>,
    /// Explain delivery or settlement was degraded. This is independent of
    /// whether any numeric lane survived and must remain visible to turn
    /// settlement consumers.
    pub(crate) capture_degraded: bool,
    pub(crate) auxiliary: Option<AttributedTokenUsage>,
    pub(crate) auxiliary_complete: bool,
    /// The runtime emitted an auxiliary snapshot, but its facts were
    /// unavailable or contradictory. Keep this separate from zero usage so a
    /// missing capture can never render as a successful zero-call judgment.
    pub(crate) auxiliary_capture_unavailable: bool,
    /// A terminal turn was observed without the auxiliary snapshot that the
    /// current runtime normally attaches to it.
    pub(crate) auxiliary_capture_missing: bool,
    pub(crate) auxiliary_capture_conflicted: bool,
    pub(crate) auxiliary_capture_truncated: bool,
    pub(crate) auxiliary_attempts: u32,
    /// Bounded, already-associated `provider (model) · operation` labels.
    /// Keeping the association prevents separate model/operation sets from
    /// implying pairings that never occurred.
    pub(crate) auxiliary_sources: Vec<String>,
}

impl UsageAttribution {
    pub(crate) fn from_explain_analyze_events(
        events: &[astra_turn_types::ExplainAnalyzeEventV1],
        primary_model: Option<String>,
        explain_analyze_degraded: bool,
    ) -> Self {
        let mut graph = astra_turn_types::ExplainAnalyzeGraphV1::default();
        for event in events {
            graph.apply(event.clone());
        }
        graph.finish_ingest();

        let mut primary_accumulator = UsageLaneAccumulator::default();
        let provider_attempt_nodes = graph
            .nodes()
            .iter()
            .filter(|node| node.kind == astra_turn_types::ExplainAnalyzeNodeKindV1::ProviderAttempt)
            .collect::<Vec<_>>();
        let primary_attempts = provider_attempt_nodes.len().try_into().unwrap_or(u32::MAX);
        let mut primary_models = BTreeSet::new();
        for node in &provider_attempt_nodes {
            if node.terminal_observed
                && !node.conflicted
                && let Some(usage) = node.usage.as_ref().filter(|usage| usage.is_valid())
            {
                primary_accumulator.add(usage);
            }
            if let Some(model) = provider_attempt_model_name(&node.label) {
                primary_models.insert(model.to_string());
            }
        }

        let scope_coverage = graph.execution_scope_coverage();
        let auxiliary_observed = scope_coverage
            .iter()
            .any(|scope| scope.auxiliary_snapshot_observed);
        // Every observed physical scope must close its own Turn boundary and
        // carry its own auxiliary snapshot. A completed earlier scope cannot
        // prove coverage for a resumed or retried later scope.
        let auxiliary_capture_missing = !scope_coverage.is_empty()
            && scope_coverage.iter().any(|scope| {
                !scope.terminal_turn_observed
                    || scope.turn_conflicted
                    || !scope.auxiliary_snapshot_observed
            });
        let auxiliary_attempts = graph.auxiliary_attempts();
        let mut auxiliary_accumulator = UsageLaneAccumulator::default();
        let mut auxiliary_sources = BTreeSet::new();
        for attempt in &auxiliary_attempts {
            if let Some(usage) = attempt.usage.as_ref().filter(|usage| usage.is_valid()) {
                auxiliary_accumulator.add(usage);
            }
            let source = format_provider_model(&attempt.provider, &attempt.model_name);
            let operation = if attempt.operation_id.trim().is_empty() {
                attempt.purpose.clone()
            } else {
                attempt.operation_id.clone()
            };
            auxiliary_sources.insert(if operation.trim().is_empty() {
                source
            } else {
                format!("{source} · {operation}")
            });
        }
        let auxiliary_capture_conflicted =
            graph.auxiliary_usage_conflict_count() > 0 || graph.auxiliary_capture_conflicted();
        let auxiliary_capture_unavailable = graph.auxiliary_usage_unavailable();
        let auxiliary_capture_truncated = graph.auxiliary_usage_truncated();
        let auxiliary = if auxiliary_observed {
            auxiliary_accumulator
                .finish()
                .or(Some(AttributedTokenUsage::default()))
        } else {
            None
        };

        let primary_model = format_model_list(primary_models.into_iter().collect())
            .or_else(|| (primary_attempts > 0).then_some(primary_model).flatten())
            .filter(|model| {
                let model = model.trim();
                !model.is_empty()
                    && !model.eq_ignore_ascii_case("auto")
                    && !model.eq_ignore_ascii_case("default")
            });

        let primary_input_complete =
            !explain_analyze_degraded && graph.primary_prompt_cache_usage().is_some();
        Self {
            primary: primary_accumulator.finish(),
            primary_input_complete,
            primary_complete: primary_input_complete
                && provider_attempt_nodes.iter().all(|node| {
                    node.usage
                        .as_ref()
                        .is_some_and(|usage| usage.output_tokens.is_some())
                }),
            primary_attempts,
            primary_model,
            capture_degraded: explain_analyze_degraded,
            auxiliary,
            auxiliary_complete: auxiliary_observed
                && !explain_analyze_degraded
                && !auxiliary_capture_unavailable
                && !auxiliary_capture_missing
                && !auxiliary_capture_conflicted
                && !auxiliary_capture_truncated
                && scope_coverage.iter().all(|scope| {
                    scope.terminal_turn_observed
                        && !scope.turn_conflicted
                        && scope.auxiliary_snapshot_observed
                })
                && auxiliary_attempts.iter().all(|attempt| {
                    matches!(
                        attempt.usage_status,
                        astra_turn_types::ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderExact
                    ) && attempt.usage.as_ref().is_some_and(|usage| usage.is_valid())
                }),
            auxiliary_capture_unavailable,
            auxiliary_capture_missing,
            auxiliary_capture_conflicted,
            auxiliary_capture_truncated,
            auxiliary_attempts: auxiliary_attempts.len().try_into().unwrap_or(u32::MAX),
            auxiliary_sources: auxiliary_sources.into_iter().collect(),
        }
    }

    pub(crate) fn has_auxiliary(&self) -> bool {
        self.auxiliary.is_some()
            || self.auxiliary_attempts > 0
            || self.auxiliary_capture_missing
            || self.auxiliary_capture_unavailable
            || self.auxiliary_capture_conflicted
            || self.auxiliary_capture_truncated
    }

    /// Whether Explain captured any usage state, even when no provider token
    /// lane is known.  A zero-valued or unavailable capture is still a fact
    /// that must survive turn settlement; it is not equivalent to an absent
    /// usage snapshot.
    pub(crate) fn has_observed_state(&self) -> bool {
        self.primary.is_some()
            || self.primary_complete
            || self.primary_attempts > 0
            || self.primary_model.is_some()
            || self.capture_degraded
            || self.has_auxiliary()
            || self.auxiliary_complete
            || !self.auxiliary_sources.is_empty()
    }

    pub(crate) fn auxiliary_summary(&self) -> Option<String> {
        if !self.has_auxiliary() {
            return None;
        }
        if self.auxiliary_attempts == 0 {
            return Some(if self.auxiliary_capture_conflicted {
                "Auxiliary usage · capture unavailable · conflicting facts".to_string()
            } else if self.auxiliary_capture_missing {
                "Auxiliary usage · capture unavailable · no snapshot".to_string()
            } else if self.auxiliary_capture_unavailable {
                "Auxiliary usage · capture unavailable · no calls reported".to_string()
            } else if self.auxiliary_capture_truncated {
                "Auxiliary usage · capture partial · no calls reported".to_string()
            } else {
                "Auxiliary usage · no calls reported".to_string()
            });
        }
        let state = if self.auxiliary_capture_conflicted {
            " · capture unavailable · conflicting facts"
        } else if self.auxiliary_complete {
            ""
        } else if self.auxiliary_capture_missing {
            " · capture unavailable · no snapshot"
        } else if self.auxiliary_capture_unavailable
            && self
                .auxiliary
                .is_none_or(|usage| usage.known_total_tokens() == 0)
        {
            " · capture unavailable"
        } else {
            " · capture partial"
        };
        let sources = if self.auxiliary_sources.is_empty() {
            "model unavailable".to_string()
        } else {
            self.auxiliary_sources.join(", ")
        };
        let tokens = self
            .auxiliary
            .map(AttributedTokenUsage::known_total_tokens)
            .filter(|tokens| *tokens > 0)
            .map(|tokens| format!(" · {} tokens", format_token_count(tokens)))
            .unwrap_or_default();
        let lanes = self
            .auxiliary
            .map(format_auxiliary_lanes)
            .filter(|lanes| !lanes.is_empty())
            .map(|lanes| format!(" · {lanes}"))
            .unwrap_or_default();
        Some(format!(
            "{} aux call{} · {}{}{}{}",
            self.auxiliary_attempts,
            if self.auxiliary_attempts == 1 {
                ""
            } else {
                "s"
            },
            sources,
            lanes,
            tokens,
            state,
        ))
    }
}

fn provider_attempt_model_name(label: &str) -> Option<&str> {
    label
        .strip_prefix("Model request ·")
        .map(str::trim)
        .filter(|model| !model.is_empty())
}

fn format_model_list(mut models: Vec<String>) -> Option<String> {
    if models.is_empty() {
        return None;
    }
    const MAX_MODELS: usize = 4;
    let omitted = models.len().saturating_sub(MAX_MODELS);
    models.truncate(MAX_MODELS);
    if omitted > 0 {
        models.push(format!("+{omitted} more"));
    }
    Some(models.join(", "))
}

fn format_provider_model(provider: &str, model: &str) -> String {
    let provider = if provider.eq_ignore_ascii_case("typesafe") {
        "Jev"
    } else if provider.trim().is_empty() {
        "Auxiliary"
    } else {
        provider
    };
    if model.trim().is_empty() {
        format!("{provider} (model unavailable)")
    } else {
        format!("{provider} ({model})")
    }
}

fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn format_auxiliary_lanes(usage: AttributedTokenUsage) -> String {
    [
        usage
            .fresh_input_tokens
            .map(|tokens| format!("in {}", format_token_count(tokens))),
        usage
            .cache_read_tokens
            .map(|tokens| format!("cache read {}", format_token_count(tokens))),
        usage
            .cache_creation_tokens
            .map(|tokens| format!("cache write {}", format_token_count(tokens))),
        usage
            .output_tokens
            .map(|tokens| format!("out {}", format_token_count(tokens))),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ")
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
    pub qualified_usage: Option<astra_turn_types::CanonicalTokenUsage>,
    pub tool_call_records: Vec<astra_services::session_journal::ToolCallRecord>,
    pub tools_used: Vec<String>,
    pub stall_events: Vec<(String, u32)>,
    pub verdict_events: Vec<VerdictEvent>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// Best-effort primary/auxiliary attribution captured from the canonical
    /// Explain facts before the stream failed. Overall counters above remain
    /// the durable accounting source.
    pub usage_attribution: UsageAttribution,
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
    /// Stable server classification for a stream failure.  A rejected
    /// admission may carry this without having created a durable Run.
    pub error_code: Option<String>,
    pub error_metadata: Option<serde_json::Value>,
    /// The server rejected the request before durable Run admission. Such a
    /// request is not a failed turn and must not be reconciled or journaled as
    /// one.
    pub admission_rejected: bool,
    pub last_heavy_checkpoint: Option<astra_pipeline::step_protocol::StepCheckpoint>,
    /// Partial text the model generated before the turn was interrupted.
    /// Preserved in conversation history so the next turn has context.
    pub partial_text: String,
    /// Exact prompt-history items captured before the failed runtime loop
    /// returned. This survives independently of `partial_text`, which is only
    /// a user-facing suffix and cannot represent tools or reasoning.
    pub run_transcript_messages: Vec<serde_json::Value>,
    /// The local client can no longer settle or observe an admitted durable
    /// run. The outer lifecycle decides whether exact-owner cancellation is
    /// appropriate; a clean internal stream detach preserves server recovery.
    pub remote_cancel_required: bool,
    /// An edge callback acknowledgement failed and this client detached.
    /// The sentence in `TurnFailure::error` states that fact only. The outer
    /// lifecycle owner adds whether the server run is still active.
    pub callback_client_detached: bool,
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

fn session_cancel_and_resume_commands(session_id: Option<&str>) -> (String, String) {
    match session_id.map(str::trim).filter(|id| !id.is_empty()) {
        Some(session_id) => (
            format!("`astra session cancel {session_id}`"),
            format!("`astra --resume {session_id}`"),
        ),
        None => (
            "`astra session cancel <session_id>`".to_string(),
            "`astra --resume <session_id>`".to_string(),
        ),
    }
}

/// Session cancellation settles the Session to `cancelled`. The same interactive
/// process keeps its current session id and does not resume, so a later turn
/// cannot be admitted until the user resumes the Session back to `active`.
pub(crate) fn session_stop_then_resume(session_id: Option<&str>) -> String {
    let (cancel_command, resume_command) = session_cancel_and_resume_commands(session_id);
    format!(
        "Wait for it to finish, or stop it with {cancel_command}, then run {resume_command} before sending another message."
    )
}

/// Interactive detach did not request server cancellation.
pub(crate) fn interactive_detach_recovery(session_id: Option<&str>) -> String {
    format!(
        "The server run was not cancelled. {}",
        session_stop_then_resume(session_id)
    )
}

/// Headless DELETE was attempted, but its settlement was not observed.
/// The run may already be cancelled server-side.
pub(crate) fn headless_unconfirmed_cancel_notice(session_id: Option<&str>) -> String {
    format!(
        "Exact server run cancellation could not be confirmed; the run may still be active. {}",
        session_stop_then_resume(session_id)
    )
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

impl TurnFailure {
    /// Observation of an internal transport failure is not a user request to
    /// cancel the durable run. This only classifies the failure; callers must
    /// still enforce their own identity/lease boundary before releasing it.
    pub(crate) fn is_clean_internal_stream_detach(&self) -> bool {
        self.partial.remote_cancel_required
            && !self.partial.callback_client_detached
            && self.partial.output_transport_failure.is_none()
            && self.partial.interruption.as_ref().is_some_and(|record| {
                matches!(
                    record.get("kind").and_then(serde_json::Value::as_str),
                    Some("stream_transport" | "stream_idle" | "executor_dropped")
                )
            })
    }
}

#[cfg(test)]
mod internal_detach_tests {
    use super::{PartialTurnData, TurnFailure};

    #[test]
    fn only_clean_internal_detach_preserves_run_control() {
        let mut failure = TurnFailure {
            error: "stream closed".into(),
            partial: PartialTurnData {
                remote_cancel_required: true,
                interruption: Some(serde_json::json!({"kind": "stream_transport"})),
                ..Default::default()
            },
        };
        assert!(failure.is_clean_internal_stream_detach());
        failure.partial.callback_client_detached = true;
        assert!(!failure.is_clean_internal_stream_detach());
        failure.partial.callback_client_detached = false;
        failure.partial.interruption = Some(serde_json::json!({"kind": "cancelled"}));
        assert!(!failure.is_clean_internal_stream_detach());
    }
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
        qualified_usage: failure.partial.qualified_usage,
        usage_attribution: failure.partial.usage_attribution.clone(),
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
    pub(crate) qualified_usage: Option<astra_turn_types::CanonicalTokenUsage>,
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
    /// Explicit attribution for user-visible summaries. The four legacy
    /// counters above remain observed run subtotals, not complete billing evidence.
    pub(crate) usage_attribution: UsageAttribution,
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
    /// Verified deferred-tool selection evidence. This is carried separately
    /// from `final_messages` because compaction may remove the originating
    /// tool-search response; it never grants authority without the current
    /// capability-scoped catalog accepting its schema digest.
    pub(crate) deferred_tool_activations: Vec<astra_turn_types::DeferredToolActivation>,
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
    /// Project the complete run-total ledger into the durable journal shape.
    /// Per-call records may be absent or only cover the Edge wrapper for a
    /// Server-owned run, so every CLI entrypoint must use this aggregate when
    /// it is internally complete.
    pub(crate) fn canonical_tool_outcomes(
        &self,
    ) -> Option<astra_services::session_journal::ToolOutcomeSummary> {
        let aggregate = self.tool_ledger_aggregate;
        if !aggregate.is_complete_for(self.tool_calls_count) {
            return None;
        }
        let classes = aggregate.result_classes;
        let outcomes = astra_services::session_journal::ToolOutcomeSummary {
            requested: aggregate.attempted,
            executed: classes.succeeded.saturating_add(classes.failed),
            succeeded: classes.succeeded,
            failed: classes.failed,
            rejected: classes.rejected,
            reused: classes.reused,
            suppressed: classes.suppressed,
            deferred: 0,
        };
        outcomes.is_consistent().then_some(outcomes)
    }

    pub(crate) fn apply_canonical_tool_outcomes(
        &self,
        event: &mut astra_services::session_journal::JournalEvent,
    ) {
        let Some(outcomes) = self.canonical_tool_outcomes() else {
            return;
        };
        event.tool_count = Some(outcomes.executed);
        event.tool_outcomes = Some(outcomes);
    }

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
            if !astra_turn_types::is_human_user_message(message) {
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
    fn effective_user_input_excludes_user_role_runtime_authority() {
        let mut authority = json!({"role": "user", "content": "runtime settlement"});
        astra_turn_types::mark_append_only_required_context(
            &mut authority,
            "final_answer_settlement",
            astra_turn_types::RuntimeAuthorityLifetime::NextAssistantDecision,
        );
        let messages = vec![json!({"role": "user", "content": "real task"}), authority];

        assert_eq!(
            effective_user_input_from_messages("real task", &messages),
            "real task"
        );
        assert_eq!(
            latest_user_input_from_messages("real task", &messages),
            "real task"
        );
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

#[cfg(test)]
mod usage_attribution_tests {
    use super::UsageAttribution;

    #[test]
    fn primary_input_coverage_is_independent_of_output() {
        let mut events = vec![
            started_primary("primary"),
            finished_primary(
                "primary",
                Some(astra_turn_types::ExplainAnalyzeTokenUsageV1 {
                    basis: astra_turn_types::ExplainAnalyzeUsageBasisV1::ProviderExact,
                    fresh_input_tokens: Some(100),
                    cache_read_tokens: Some(900),
                    cache_creation_tokens: Some(0),
                    output_tokens: None,
                }),
            ),
            auxiliary_event(true, vec![]),
        ];
        let usage = UsageAttribution::from_explain_analyze_events(&events, None, false);
        assert!(usage.primary_input_complete);
        assert!(!usage.primary_complete);
        let projection = crate::cli::turn::turn_reporting::project_primary_usage(&usage, false);
        assert_eq!(
            crate::cli::turn::turn_reporting::format_primary_usage_summary(
                projection.fresh_input_tokens,
                projection.output_tokens,
                projection.cache_read_tokens,
                projection.cache_creation_tokens,
                projection.observed,
                projection.complete,
                projection.input_complete,
            )
            .as_deref(),
            Some("≥1.0k tokens · 90% cached")
        );
        assert!(
            !UsageAttribution::from_explain_analyze_events(&events, None, true)
                .primary_input_complete
        );
        events[1].usage.as_mut().unwrap().cache_creation_tokens = None;
        assert!(
            !UsageAttribution::from_explain_analyze_events(&events, None, false)
                .primary_input_complete
        );
    }

    fn finished_primary(
        node_id: &str,
        usage: Option<astra_turn_types::ExplainAnalyzeTokenUsageV1>,
    ) -> astra_turn_types::ExplainAnalyzeEventV1 {
        astra_turn_types::ExplainAnalyzeEventV1 {
            schema_version: astra_turn_types::EXPLAIN_ANALYZE_SCHEMA_VERSION,
            auxiliary_details: None,
            event_id: format!("{node_id}/finished"),
            run_id: "run-1".into(),
            turn_id: "turn-1".into(),
            node_id: node_id.into(),
            parent_node_id: None,
            dependency_node_ids: Vec::new(),
            producer_id: "test".into(),
            clock_domain_id: "clock-1".into(),
            kind: astra_turn_types::ExplainAnalyzeNodeKindV1::ProviderAttempt,
            round_index: Some(0),
            attempt_index: Some(0),
            label: "Model request · deepseek-flash".into(),
            transition: astra_turn_types::ExplainAnalyzeTransitionV1::Finished,
            elapsed_ms: 10,
            start_elapsed_ms: Some(0),
            duration_ms: Some(10),
            outcome: Some(astra_turn_types::ExplainAnalyzeOutcomeV1::Succeeded),
            usage,
            auxiliary_usage: None,
            context: None,
            coverage_gaps: Vec::new(),
        }
    }

    fn started_primary(node_id: &str) -> astra_turn_types::ExplainAnalyzeEventV1 {
        let mut event = finished_primary(node_id, None);
        event.event_id = format!("{node_id}/started");
        event.transition = astra_turn_types::ExplainAnalyzeTransitionV1::Started;
        event.elapsed_ms = 0;
        event.start_elapsed_ms = None;
        event.duration_ms = None;
        event.outcome = None;
        event.usage = None;
        event
    }

    fn auxiliary_attempt(
        attempt_id: &str,
        status: astra_turn_types::ExplainAnalyzeAuxiliaryUsageStatusV1,
        usage: Option<astra_turn_types::ExplainAnalyzeTokenUsageV1>,
    ) -> astra_turn_types::ExplainAnalyzeAuxiliaryAttemptV1 {
        astra_turn_types::ExplainAnalyzeAuxiliaryAttemptV1 {
            attempt_id: attempt_id.into(),
            usage_status: status,
            provider: "typesafe".into(),
            offering_id: "jev-offering".into(),
            model_name: "jev-1.13.0".into(),
            purpose: "introspection".into(),
            operation_id: "request_judgment".into(),
            usage,
        }
    }

    fn auxiliary_event(
        available: bool,
        attempts: Vec<astra_turn_types::ExplainAnalyzeAuxiliaryAttemptV1>,
    ) -> astra_turn_types::ExplainAnalyzeEventV1 {
        astra_turn_types::ExplainAnalyzeEventV1 {
            schema_version: astra_turn_types::EXPLAIN_ANALYZE_SCHEMA_VERSION,
            auxiliary_details: None,
            event_id: "auxiliary/turn/finished".into(),
            run_id: "run-1".into(),
            turn_id: "turn-1".into(),
            node_id: "turn".into(),
            parent_node_id: None,
            dependency_node_ids: Vec::new(),
            producer_id: "test".into(),
            clock_domain_id: "clock-1".into(),
            kind: astra_turn_types::ExplainAnalyzeNodeKindV1::Turn,
            round_index: None,
            attempt_index: None,
            label: "User turn".into(),
            transition: astra_turn_types::ExplainAnalyzeTransitionV1::Finished,
            elapsed_ms: 20,
            start_elapsed_ms: Some(0),
            duration_ms: Some(20),
            outcome: Some(astra_turn_types::ExplainAnalyzeOutcomeV1::Completed),
            usage: None,
            auxiliary_usage: Some(Box::new(astra_turn_types::ExplainAnalyzeAuxiliaryUsageV1 {
                available,
                truncated: false,
                attempts,
            })),
            context: None,
            coverage_gaps: Vec::new(),
        }
    }

    #[test]
    fn primary_and_auxiliary_usage_are_not_mixed() {
        let primary = finished_primary(
            "primary",
            Some(astra_turn_types::ExplainAnalyzeTokenUsageV1 {
                basis: astra_turn_types::ExplainAnalyzeUsageBasisV1::ProviderExact,
                fresh_input_tokens: Some(100),
                cache_read_tokens: Some(900),
                cache_creation_tokens: Some(20),
                output_tokens: Some(10),
            }),
        );
        let auxiliary = astra_turn_types::ExplainAnalyzeEventV1 {
            schema_version: astra_turn_types::EXPLAIN_ANALYZE_SCHEMA_VERSION,
            auxiliary_details: None,
            event_id: "turn/finished".into(),
            run_id: "run-1".into(),
            turn_id: "turn-1".into(),
            node_id: "turn".into(),
            parent_node_id: None,
            dependency_node_ids: Vec::new(),
            producer_id: "test".into(),
            clock_domain_id: "clock-1".into(),
            kind: astra_turn_types::ExplainAnalyzeNodeKindV1::Turn,
            round_index: None,
            attempt_index: None,
            label: "User turn".into(),
            transition: astra_turn_types::ExplainAnalyzeTransitionV1::Finished,
            elapsed_ms: 20,
            start_elapsed_ms: Some(0),
            duration_ms: Some(20),
            outcome: Some(astra_turn_types::ExplainAnalyzeOutcomeV1::Completed),
            usage: None,
            auxiliary_usage: Some(Box::new(astra_turn_types::ExplainAnalyzeAuxiliaryUsageV1 {
                available: true,
                truncated: false,
                attempts: vec![astra_turn_types::ExplainAnalyzeAuxiliaryAttemptV1 {
                    attempt_id: "aux-1".into(),
                    usage_status:
                        astra_turn_types::ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderExact,
                    provider: "typesafe".into(),
                    offering_id: "jev-offering".into(),
                    model_name: "jev-1.13.0".into(),
                    purpose: "introspection".into(),
                    operation_id: "request_judgment".into(),
                    usage: Some(astra_turn_types::ExplainAnalyzeTokenUsageV1 {
                        basis: astra_turn_types::ExplainAnalyzeUsageBasisV1::ProviderExact,
                        fresh_input_tokens: Some(7),
                        cache_read_tokens: Some(3),
                        cache_creation_tokens: Some(0),
                        output_tokens: Some(2),
                    }),
                }],
            })),
            context: None,
            coverage_gaps: Vec::new(),
        };

        let attribution = UsageAttribution::from_explain_analyze_events(
            &[started_primary("primary"), primary, auxiliary],
            Some("deepseek-flash".into()),
            false,
        );
        let primary = attribution.primary.expect("primary usage");
        let auxiliary = attribution.auxiliary.expect("auxiliary usage");
        assert_eq!(primary.fresh_input_tokens, Some(100));
        assert_eq!(primary.cache_read_tokens, Some(900));
        assert_eq!(primary.known_total_tokens(), 1_030);
        assert_eq!(auxiliary.known_total_tokens(), 12);
        assert!(attribution.primary_complete);
        assert!(attribution.auxiliary_complete);
        let summary = attribution.auxiliary_summary().expect("aux summary");
        assert!(summary.contains("Jev (jev-1.13.0)"));
        assert!(summary.contains("request_judgment"));
        assert!(summary.contains("in 7"));
        assert!(summary.contains("out 2"));
    }

    #[test]
    fn timing_coverage_gaps_do_not_invalidate_complete_token_usage() {
        let primary = finished_primary(
            "primary",
            Some(astra_turn_types::ExplainAnalyzeTokenUsageV1 {
                basis: astra_turn_types::ExplainAnalyzeUsageBasisV1::ProviderExact,
                fresh_input_tokens: Some(100),
                cache_read_tokens: Some(900),
                cache_creation_tokens: Some(0),
                output_tokens: Some(10),
            }),
        );
        let mut terminal = auxiliary_event(true, Vec::new());
        terminal.auxiliary_usage = None;
        terminal.coverage_gaps = vec![
            astra_turn_types::ExplainAnalyzeCoverageGapV1::ApprovalWaitIntervals,
            astra_turn_types::ExplainAnalyzeCoverageGapV1::ChildRunIntervals,
            astra_turn_types::ExplainAnalyzeCoverageGapV1::FirstTokenLatency,
            astra_turn_types::ExplainAnalyzeCoverageGapV1::ProviderRetryBackoff,
            astra_turn_types::ExplainAnalyzeCoverageGapV1::ToolIoWaitIntervals,
            astra_turn_types::ExplainAnalyzeCoverageGapV1::UserInputWaitIntervals,
        ];

        let attribution = UsageAttribution::from_explain_analyze_events(
            &[started_primary("primary"), primary, terminal],
            Some("deepseek-flash".into()),
            false,
        );

        assert!(
            attribution.primary_complete,
            "timing coverage is independent from provider token coverage"
        );
    }

    #[test]
    fn timing_coverage_gaps_do_not_hide_missing_token_lanes() {
        let missing_usage = finished_primary("primary", None);
        let mut terminal = auxiliary_event(true, Vec::new());
        terminal.auxiliary_usage = None;
        terminal.coverage_gaps = vec![
            astra_turn_types::ExplainAnalyzeCoverageGapV1::FirstTokenLatency,
            astra_turn_types::ExplainAnalyzeCoverageGapV1::ToolIoWaitIntervals,
        ];

        let attribution = UsageAttribution::from_explain_analyze_events(
            &[started_primary("primary"), missing_usage, terminal],
            Some("deepseek-flash".into()),
            false,
        );

        assert!(!attribution.primary_complete);
    }

    #[test]
    fn later_scope_without_terminal_cannot_inherit_prior_completion() {
        let first_primary = finished_primary(
            "primary-first",
            Some(astra_turn_types::ExplainAnalyzeTokenUsageV1 {
                basis: astra_turn_types::ExplainAnalyzeUsageBasisV1::ProviderExact,
                fresh_input_tokens: Some(100),
                cache_read_tokens: Some(900),
                cache_creation_tokens: Some(0),
                output_tokens: Some(10),
            }),
        );
        let first_terminal = auxiliary_event(true, Vec::new());
        let mut second_started = started_primary("primary-second");
        let mut second_primary = finished_primary(
            "primary-second",
            Some(astra_turn_types::ExplainAnalyzeTokenUsageV1 {
                basis: astra_turn_types::ExplainAnalyzeUsageBasisV1::ProviderExact,
                fresh_input_tokens: Some(120),
                cache_read_tokens: Some(880),
                cache_creation_tokens: Some(0),
                output_tokens: Some(11),
            }),
        );
        // A retry/resume can keep the logical turn id while opening a new
        // physical clock scope. The second provider request finished, but
        // the executor exited before the second Turn terminal was emitted.
        second_started.clock_domain_id = "clock-2".into();
        second_primary.clock_domain_id = "clock-2".into();

        let attribution = UsageAttribution::from_explain_analyze_events(
            &[
                started_primary("primary-first"),
                first_primary,
                first_terminal,
                second_started,
                second_primary,
            ],
            Some("deepseek-flash".into()),
            false,
        );

        assert!(!attribution.primary_complete);
        assert!(attribution.auxiliary_capture_missing);
        assert!(!attribution.auxiliary_complete);
    }

    #[test]
    fn partial_auxiliary_capture_keeps_known_lanes_and_association() {
        let exact = auxiliary_attempt(
            "aux-exact",
            astra_turn_types::ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderExact,
            Some(astra_turn_types::ExplainAnalyzeTokenUsageV1 {
                basis: astra_turn_types::ExplainAnalyzeUsageBasisV1::ProviderExact,
                fresh_input_tokens: Some(7),
                cache_read_tokens: Some(3),
                cache_creation_tokens: Some(0),
                output_tokens: Some(2),
            }),
        );
        let unavailable = auxiliary_attempt(
            "aux-unavailable",
            astra_turn_types::ExplainAnalyzeAuxiliaryUsageStatusV1::Unavailable,
            None,
        );
        let attribution = UsageAttribution::from_explain_analyze_events(
            &[auxiliary_event(true, vec![exact, unavailable])],
            None,
            false,
        );

        assert!(!attribution.auxiliary_complete);
        assert!(!attribution.auxiliary_capture_unavailable);
        assert_eq!(
            attribution
                .auxiliary
                .expect("known auxiliary lane")
                .known_total_tokens(),
            12
        );
        let summary = attribution.auxiliary_summary().expect("aux summary");
        assert!(summary.contains("Jev (jev-1.13.0) · request_judgment"));
        assert!(summary.contains("12 tokens"));
        assert!(summary.contains("capture partial"));
    }

    #[test]
    fn degraded_explain_capture_never_claims_complete_auxiliary_usage() {
        let exact = auxiliary_attempt(
            "aux-exact",
            astra_turn_types::ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderExact,
            Some(astra_turn_types::ExplainAnalyzeTokenUsageV1 {
                basis: astra_turn_types::ExplainAnalyzeUsageBasisV1::ProviderExact,
                fresh_input_tokens: Some(7),
                cache_read_tokens: Some(3),
                cache_creation_tokens: Some(0),
                output_tokens: Some(2),
            }),
        );
        let attribution = UsageAttribution::from_explain_analyze_events(
            &[auxiliary_event(true, vec![exact])],
            None,
            true,
        );

        assert!(!attribution.auxiliary_complete);
        assert!(
            attribution
                .auxiliary_summary()
                .is_some_and(|summary| summary.contains("capture partial"))
        );
    }

    #[test]
    fn degraded_capture_is_retained_without_numeric_usage() {
        let attribution = UsageAttribution::from_explain_analyze_events(
            &[started_primary("primary")],
            Some("deepseek-flash".into()),
            true,
        );

        assert!(attribution.capture_degraded);
        assert!(attribution.has_observed_state());
        assert!(!attribution.primary_complete);
    }

    #[test]
    fn unavailable_auxiliary_snapshot_is_not_rendered_as_zero_calls() {
        let attribution = UsageAttribution::from_explain_analyze_events(
            &[auxiliary_event(false, Vec::new())],
            None,
            false,
        );

        assert!(attribution.auxiliary_capture_unavailable);
        assert_eq!(
            attribution.auxiliary_summary().as_deref(),
            Some("Auxiliary usage · capture unavailable · no calls reported")
        );
    }

    #[test]
    fn empty_auxiliary_snapshot_means_zero_calls() {
        let attribution = UsageAttribution::from_explain_analyze_events(
            &[auxiliary_event(true, Vec::new())],
            None,
            false,
        );

        assert!(!attribution.auxiliary_capture_unavailable);
        assert!(!attribution.auxiliary_capture_missing);
        assert_eq!(
            attribution.auxiliary_summary().as_deref(),
            Some("Auxiliary usage · no calls reported")
        );
    }

    #[test]
    fn missing_auxiliary_snapshot_is_not_rendered_as_zero_calls() {
        let mut terminal = auxiliary_event(true, Vec::new());
        terminal.auxiliary_usage = None;
        let attribution = UsageAttribution::from_explain_analyze_events(&[terminal], None, false);

        assert!(attribution.auxiliary_capture_missing);
        assert_eq!(
            attribution.auxiliary_summary().as_deref(),
            Some("Auxiliary usage · capture unavailable · no snapshot")
        );
    }

    #[test]
    fn auxiliary_conflict_remains_visible_without_surviving_attempts() {
        let first = auxiliary_event(true, Vec::new());
        let mut conflicting = auxiliary_event(false, Vec::new());
        conflicting.event_id = first.event_id.clone();
        let attribution =
            UsageAttribution::from_explain_analyze_events(&[first, conflicting], None, false);

        assert!(attribution.auxiliary_capture_conflicted);
        assert_eq!(attribution.auxiliary_attempts, 0);
        assert_eq!(
            attribution.auxiliary_summary().as_deref(),
            Some("Auxiliary usage · capture unavailable · conflicting facts")
        );
    }

    #[test]
    fn partial_primary_capture_preserves_known_lanes_without_claiming_complete() {
        let partial = finished_primary(
            "primary",
            Some(astra_turn_types::ExplainAnalyzeTokenUsageV1 {
                basis: astra_turn_types::ExplainAnalyzeUsageBasisV1::ProviderPartial,
                fresh_input_tokens: Some(100),
                cache_read_tokens: Some(900),
                cache_creation_tokens: None,
                output_tokens: Some(10),
            }),
        );
        let mut terminal = auxiliary_event(true, Vec::new());
        terminal.event_id = "turn/terminal".into();
        terminal.auxiliary_usage = None;
        let attribution = UsageAttribution::from_explain_analyze_events(
            &[started_primary("primary"), partial, terminal],
            Some("deepseek-flash".into()),
            false,
        );

        let primary = attribution.primary.expect("known primary lanes");
        assert_eq!(primary.fresh_input_tokens, Some(100));
        assert_eq!(primary.cache_read_tokens, Some(900));
        assert_eq!(primary.output_tokens, Some(10));
        assert_eq!(primary.cache_creation_tokens, None);
        assert!(!attribution.primary_complete);
    }

    #[test]
    fn no_explain_evidence_does_not_infer_primary_attribution() {
        let attribution = UsageAttribution::from_explain_analyze_events(
            &[],
            Some("deepseek-flash".into()),
            false,
        );

        assert!(attribution.primary.is_none());
        assert!(!attribution.primary_complete);
        assert!(attribution.primary_model.is_none());
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
            usage_attribution: UsageAttribution::default(),
            qualified_usage: None,
            tool_calls_count: 0,
            tool_ledger_aggregate: Default::default(),
            visible_tools: vec![],
            selected_skills: vec![],
            tools_used: vec![],
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
            deferred_tool_activations: Vec::new(),
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
