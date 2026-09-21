//! Budget-adaptive runtime introspection for the always-load `introspect` tool.
//!
//! The LLM calls `introspect` to query its own session state — token pressure,
//! cache efficiency, tool health, active alerts, and working memory. Output
//! depth follows the normalized observation-plane request.

pub mod cache_diagnosis;
mod model_projection;
mod observation;
mod request;

use serde::{Deserialize, Serialize};

use crate::injection_tracking::{ChannelFreshness, ChannelStatus, InjectionChannel};
use astra_core::{ObservationDepth, ObservationFacet, ObservationHorizon};
pub use observation::{
    INTROSPECT_REPORT_SCHEMA_VERSION, IntrospectReport, build_introspect_report,
};
pub use request::{IntrospectDepth, IntrospectFormat, IntrospectRequest};

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

/// Input snapshot provided by the runtime to the introspect renderer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntrospectSnapshot {
    /// Canonical facts from the latest successfully ingested provider round.
    /// `None` means no provider round has been observed yet; consumers must
    /// not manufacture zero-valued progress or capacity from that absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_feedback: Option<crate::context_feedback::RuntimeFeedbackFrame>,
    /// Number of user/session turns elapsed since this snapshot was captured.
    /// Live snapshots use 0; holders that serve older snapshots can increment
    /// this before rendering.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub snapshot_age_turns: u32,
    pub alerts: Vec<String>,
    pub tool_health: Vec<ToolHealthEntry>,
    pub working_memory_summary: String,
    /// Host-provided lifecycle context. In the CLI this is the same
    /// turn-start plan/task/session block injected into the prompt; it is not
    /// a live mid-turn projection of mutations that happened after the round
    /// began.
    #[serde(default)]
    pub lifecycle_summary: String,
    /// Capacity-provider coverage for this runtime surface. This is separate
    /// from data coverage: it describes which execution/provider classes are
    /// ready, unbound, degraded, or unavailable for tool visibility.
    #[serde(default)]
    pub capacity_provider_coverage: Vec<CapacityProviderCoverageEntry>,
    /// Per-tool admission decisions for the current runtime surface. This is
    /// runtime metadata for introspect/UI/audit only; prompt-visible tool
    /// schemas remain canonical and do not include provider placement.
    #[serde(default)]
    pub tool_admission: Vec<ToolAdmissionSnapshotEntry>,
    /// Recent governed semantic-read decisions projected from authoritative
    /// tool-result evidence. This is bounded runtime evidence, not a second
    /// cache state store.
    #[serde(default)]
    pub semantic_cache_decisions: Vec<SemanticCacheDecisionSnapshotEntry>,
    /// Owner-scoped durable invocation, archive, artifact-reference, and
    /// reconciliation evidence loaded from the execution ledger. This is a
    /// read-only projection and never becomes execution authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_lifecycle: Option<InvocationLifecycleSnapshot>,
    /// On-demand session-scoped physical judgment attempts from the service
    /// inference ledger, independent of the live round snapshot's cutoff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judgment_usage: Option<JudgmentUsageSnapshot>,
    /// Separate C3 semantic facts; never physical attempt counts or adoption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_judgments:
        Option<astra_services::semantic_judgment_observation::SemanticJudgmentView>,
    /// Shared read-only evaluation/application projection for large tool
    /// results. This never becomes execution authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_judgments:
        Option<astra_services::tool_result_selection_observation::ToolResultJudgmentView>,

    // ── Task #46: enhanced self-awareness ──
    /// Summary of the most recent LLM rounds (in-memory ring). Available
    /// regardless of `full_llm_capture` setting. Populated by
    /// `AgenticLoopState.recent_rounds`. Feeds `facet=recent`.
    #[serde(default)]
    pub recent_rounds: Vec<RoundSnapshotEntry>,
    /// Per-step latency attribution derived from step events. Included in
    /// diagnostic/full session renders and helps distinguish model wait from
    /// tool/DB time.
    #[serde(default)]
    pub step_latency: Vec<StepLatencySnapshotEntry>,
    /// Currently-pending volatile injections scheduled for the next LLM
    /// call (policy advisories, working-set snapshots, …). Lets the agent
    /// inspect pending runtime feedback. Feeds `facet=volatile`.
    #[serde(default)]
    pub volatile_pending: Vec<VolatileSnapshotEntry>,
    /// Current stall / loop-guard events and advisory labels.
    /// Feeds `facet=stall` alongside circuit-breaker state.
    #[serde(default)]
    pub stall_state: StallSnapshotSummary,
    /// Per-channel freshness of runtime-injected prompt signals
    /// (recent_failing_tests, outcome_bias, lessons, volatile_pending).
    /// Populated by the agentic loop from `AgenticLoopState.injection_history`
    /// at the start of each round. Feeds `facet=noise`. Empty when
    /// the runtime has not yet observed any round.
    #[serde(default)]
    pub injection_freshness: Vec<ChannelFreshness>,
    /// Current round index at snapshot time — used to interpret
    /// `rounds_alive` in the freshness report. 0 when unknown.
    #[serde(default)]
    pub current_round: u32,

    /// Recent tool errors with previews — feeds `facet=errors`.
    #[serde(default)]
    pub tool_errors: Vec<ToolErrorEntry>,

    /// Bridge circuit breaker state — surfaced in stall/full renders.
    #[serde(default)]
    pub circuit_breaker: Option<CircuitBreakerSnapshot>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JudgmentUsageCoverage {
    Available,
    CaptureTruncated,
    #[default]
    NotObserved,
    NoPool,
    Timeout,
    QueryFailed,
    LedgerUnavailable,
    LocalCaptureUnavailable,
    SourceExcluded,
}

/// Bounded projection of the service ledger, never a judgment or execution
/// authority. Nullable usage remains nullable, including cache input buckets.
pub use astra_services::reflect::JudgmentUsageScope;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct JudgmentUsageSnapshot {
    pub scope: JudgmentUsageScope,
    pub capture_incomplete: bool,
    pub coverage: JudgmentUsageCoverage,
    pub observed_attempts: Option<usize>,
    pub attempts_without_complete_usage: Option<usize>,
    /// Lower-bound totals across all captured attempts, before display
    /// truncation. None means ledger coverage is unavailable, not zero usage.
    pub known_input_tokens: Option<u128>,
    pub known_output_tokens: Option<u128>,
    pub input_complete: bool,
    pub output_complete: bool,
    pub omitted_attempts: usize,
    pub truncated_identity_fields: usize,
    pub attempts: Vec<astra_turn_types::ExplainAnalyzeAuxiliaryAttemptV1>,
    pub groups: Vec<JudgmentUsageGroup>,
    pub omitted_groups: usize,
}

/// Identity-scoped totals across all captured physical attempts, including
/// retries whose individual detail is omitted from the report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JudgmentUsageGroup {
    pub provider: String,
    pub offering_id: String,
    pub model: String,
    pub operation: String,
    pub attempts: usize,
    pub known_input_tokens: u128,
    pub known_output_tokens: u128,
    pub input_complete: bool,
    pub output_complete: bool,
}

impl JudgmentUsageSnapshot {
    pub fn unavailable(coverage: JudgmentUsageCoverage) -> Self {
        Self {
            coverage,
            ..Self::default()
        }
    }

    pub fn from_ledger(facts: astra_turn_types::ExplainAnalyzeAuxiliaryUsageV1) -> Self {
        if !facts.available {
            return Self::unavailable(JudgmentUsageCoverage::LedgerUnavailable);
        }
        let incomplete = facts
            .attempts
            .iter()
            .filter(|attempt| {
                attempt.usage.as_ref().is_none_or(|usage| {
                    usage.fresh_input_tokens.is_none()
                        || usage.cache_read_tokens.is_none()
                        || usage.cache_creation_tokens.is_none()
                        || usage.output_tokens.is_none()
                })
            })
            .count();
        let mut known_input_tokens = 0_u128;
        let mut known_output_tokens = 0_u128;
        let mut input_complete = !facts.truncated;
        let mut output_complete = !facts.truncated;
        let mut groups = std::collections::BTreeMap::new();
        for attempt in &facts.attempts {
            let group = groups
                .entry((
                    attempt.provider.clone(),
                    attempt.offering_id.clone(),
                    attempt.model_name.clone(),
                    attempt.operation_id.clone(),
                ))
                .or_insert_with(|| JudgmentUsageGroup {
                    provider: attempt.provider.clone(),
                    offering_id: attempt.offering_id.clone(),
                    model: attempt.model_name.clone(),
                    operation: attempt.operation_id.clone(),
                    attempts: 0,
                    known_input_tokens: 0,
                    known_output_tokens: 0,
                    input_complete: !facts.truncated,
                    output_complete: !facts.truncated,
                });
            group.attempts += 1;
            let usage = attempt.usage.as_ref();
            for value in [
                usage.and_then(|u| u.fresh_input_tokens),
                usage.and_then(|u| u.cache_read_tokens),
                usage.and_then(|u| u.cache_creation_tokens),
            ] {
                if let Some(value) = value {
                    known_input_tokens += u128::from(value);
                    group.known_input_tokens += u128::from(value);
                } else {
                    input_complete = false;
                    group.input_complete = false;
                }
            }
            if let Some(value) = usage.and_then(|u| u.output_tokens) {
                known_output_tokens += u128::from(value);
                group.known_output_tokens += u128::from(value);
            } else {
                output_complete = false;
                group.output_complete = false;
            }
        }
        Self {
            coverage: if facts.truncated {
                JudgmentUsageCoverage::CaptureTruncated
            } else {
                JudgmentUsageCoverage::Available
            },
            observed_attempts: Some(facts.attempts.len()),
            attempts_without_complete_usage: Some(incomplete),
            known_input_tokens: Some(known_input_tokens),
            known_output_tokens: Some(known_output_tokens),
            input_complete,
            output_complete,
            attempts: facts.attempts,
            groups: groups.into_values().collect(),
            ..Self::default()
        }
        .bounded(ObservationDepth::Forensic)
    }

    pub fn bounded(&self, depth: ObservationDepth) -> Self {
        let limit = match depth {
            ObservationDepth::Hint => 2,
            ObservationDepth::Summary => 8,
            ObservationDepth::Diagnostic => 16,
            ObservationDepth::Forensic => 32,
        };
        let mut result = Self {
            attempts: self.attempts.iter().take(limit).cloned().collect(),
            groups: self.groups.iter().take(limit).cloned().collect(),
            omitted_groups: self
                .omitted_groups
                .saturating_add(self.groups.len().saturating_sub(limit)),
            omitted_attempts: self
                .omitted_attempts
                .saturating_add(self.attempts.len().saturating_sub(limit)),
            ..self.clone()
        };
        let fields = result
            .attempts
            .iter_mut()
            .flat_map(|attempt| {
                [
                    &mut attempt.attempt_id,
                    &mut attempt.provider,
                    &mut attempt.offering_id,
                    &mut attempt.model_name,
                    &mut attempt.purpose,
                    &mut attempt.operation_id,
                ]
            })
            .chain(result.groups.iter_mut().flat_map(|group| {
                [
                    &mut group.provider,
                    &mut group.offering_id,
                    &mut group.model,
                    &mut group.operation,
                ]
            }));
        for field in fields {
            if field.chars().nth(128).is_some() {
                let end = field
                    .char_indices()
                    .nth(127)
                    .map(|(end, _)| end)
                    .unwrap_or(0);
                field.truncate(end);
                field.push('…');
                result.truncated_identity_fields += 1;
            }
        }
        result
    }

    pub fn render(&self) -> String {
        let mut out = format!(
            "Judgment physical usage: {} coverage={:?} observed_attempts={:?} incomplete_usage_attempts={:?} omitted_attempts={} truncated_identity_fields={}",
            self.scope.render(),
            self.coverage,
            self.observed_attempts,
            self.attempts_without_complete_usage,
            self.omitted_attempts,
            self.truncated_identity_fields,
        );
        if self.coverage == JudgmentUsageCoverage::CaptureTruncated {
            out.push_str(" capture_truncated=true; attempt counts cover captured rows only; additional physical attempts were omitted (count unknown)");
        }
        if self.capture_incomplete {
            out.push_str(" capture_incomplete=true; missing historical attempts unknown; totals are lower bounds, independent of truncation");
        }
        let total = |known: Option<u128>, complete: bool| match known {
            None => "unknown".into(),
            Some(value) if complete => value.to_string(),
            Some(value) => format!("at_least_{value} (incomplete)"),
        };
        out.push_str(&format!(
            " total_input={} total_output={} input_complete={} output_complete={} omitted_groups={}",
            total(self.known_input_tokens, self.input_complete),
            total(self.known_output_tokens, self.output_complete),
            self.input_complete,
            self.output_complete,
            self.omitted_groups,
        ));
        for group in &self.groups {
            out.push_str(&format!(
                "\n- group provider={} offering={} model={} operation={} attempts={} input={} output={} input_complete={} output_complete={}",
                group.provider, group.offering_id, group.model, group.operation, group.attempts,
                total(Some(group.known_input_tokens), group.input_complete),
                total(Some(group.known_output_tokens), group.output_complete),
                group.input_complete, group.output_complete,
            ));
        }
        for attempt in &self.attempts {
            let usage = attempt.usage.as_ref();
            let parts = usage.map(|u| {
                [
                    u.fresh_input_tokens,
                    u.cache_read_tokens,
                    u.cache_creation_tokens,
                ]
            });
            let input = parts.map_or_else(
                || "unknown".into(),
                |parts| {
                    if parts.iter().all(Option::is_none) {
                        return "unknown".into();
                    }
                    let sum: u128 = parts.iter().flatten().map(|v| u128::from(*v)).sum();
                    if parts.iter().any(Option::is_none) {
                        format!("at_least_{sum} (incomplete)")
                    } else {
                        sum.to_string()
                    }
                },
            );
            let output = usage
                .and_then(|u| u.output_tokens)
                .map_or_else(|| "unknown".into(), |v| v.to_string());
            out.push_str(&format!(
                "\n- attempt={} provider={} offering={} model={} operation={} usage={:?} input={} output={}",
                attempt.attempt_id, attempt.provider, attempt.offering_id, attempt.model_name,
                attempt.operation_id, attempt.usage_status, input, output,
            ));
        }
        out
    }
}

/// Per-round summary surfaced through `introspect(facet=recent)`.
/// Mirrors `RecentRoundSummary` in the runtime but serializes cleanly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoundSnapshotEntry {
    pub turn: u32,
    pub round: u32,
    pub provider: String,
    pub model: String,
    pub prompt_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub completion_tokens: u64,
    pub tool_calls_returned: u32,
    pub tool_call_names: Vec<String>,
    pub duration_ms: u64,
    pub finish_reason: Option<String>,
}

/// Per-step latency attribution surfaced through `introspect`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StepLatencySnapshotEntry {
    pub step_id: String,
    pub total_ms: Option<u64>,
    pub pre_tool_wait_ms: Option<u64>,
    pub first_tool_name: Option<String>,
    pub tool_call_count: u32,
    pub skipped_tool_count: u32,
    pub tool_execution_ms: u64,
    pub max_tool_execution_ms: u64,
    pub terminal_event_kind: Option<String>,
    pub dominant_phase: String,
}

/// Single entry in the volatile lane at introspect time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VolatileSnapshotEntry {
    /// Kind as a short string ("WorkingSet", "PolicyAdvisory", …). Keeps the
    /// core crate dependency-free from the runtime's `VolatileKind`.
    pub kind: String,
    /// Content preview — full text (the renderers may truncate at
    /// display time based on detail level).
    pub content: String,
    /// Round the injection was produced in.
    pub round_index: u32,
}

/// Stall / loop-guard state at introspect time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StallSnapshotSummary {
    pub events: Vec<String>,
    /// Total circuit-breaker introspection emissions this turn.
    pub introspection_count: u32,
    /// Advisory labels emitted this turn; these are observations, not
    /// evidence that a correction was required or ignored.
    #[serde(default)]
    pub advisory_signals: Vec<String>,
}

/// Per-tool health entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolHealthEntry {
    pub name: String,
    /// Calls that reached the executor.
    pub calls: u32,
    /// Executor failures. Input rejects are reported separately.
    pub errors: u32,
    /// Requests rejected before execution because their arguments failed
    /// schema/input validation. This is a caller-quality signal, not a tool
    /// reliability signal.
    pub input_validation_failures: u32,
    pub avg_ms: u64,
    #[serde(default)]
    pub avoidance_advised: bool,
    #[serde(default)]
    pub consecutive_failures: u32,
    #[serde(default)]
    pub last_failure_category: Option<String>,
}

/// Recent tool error entry for `facet=errors`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolErrorEntry {
    pub tool: String,
    pub signature_hint: String,
    pub failure_category: Option<String>,
    pub error_preview: Option<String>,
    pub at_epoch: u64,
    /// Raw error message (not classified, not truncated).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error_message: String,
    /// File path for read_file/write_file/str_replace errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    /// Line range for read_file errors (e.g., "200-400").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_range: Option<String>,
    /// Turn index when the error occurred.
    #[serde(default)]
    pub turn: u32,
    /// Round index within the turn.
    #[serde(default)]
    pub round: u32,
}

/// Bridge circuit breaker state snapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CircuitBreakerSnapshot {
    pub state: String,
    pub failure_count: u64,
    pub success_count: u64,
    pub consecutive_failures: u64,
}

pub use astra_runtime_env::CapacityProviderCoverageEntry;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolAdmissionSnapshotEntry {
    pub tool_name: String,
    /// Whether this exact tool schema was present on the most recent provider
    /// request. `None` means the host has not observed a provider wire surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_visible: Option<bool>,
    /// Route/provider readiness is a candidate-plane fact, not proof that the
    /// schema was sent to the model. Keep the legacy field name on the wire,
    /// but render it with its precise scope.
    pub visible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_offer_id: Option<String>,
    pub selected_route: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden_reason: Option<String>,
    #[serde(default)]
    pub candidates: Vec<ToolAdmissionCandidateSnapshotEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticCacheDecisionSnapshotEntry {
    pub tool_name: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvocationLifecycleSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub hot_total: u64,
    pub prepared: u64,
    pub dispatched: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub rejected: u64,
    pub outcome_unknown: u64,
    pub rejected_without_dispatch: u64,
    pub archive_chunks: u64,
    pub durable_artifact_references: u64,
    pub reconciliation_events: u64,
    pub compaction_deferred_events: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_cursor_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_cursor_updated_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolAdmissionCandidateSnapshotEntry {
    pub offer_id: String,
    pub provider_type: String,
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub executor_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub placement: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scope: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub authority: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub schema_digest: String,
    pub route: String,
    pub readiness: String,
    pub selected: bool,
    pub reason: String,
}

/// Text output depth chosen from the normalized observation-plane request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntrospectTextDepth {
    /// Full diagnostics (~500-800 tokens output).
    Full,
    /// Key metrics + top alerts (~150-250 tokens).
    Summary,
    /// One-liner (~30-50 tokens).
    Hint,
}

/// Select the same source-scoped semantic trace view for text and JSON.
fn judgment_source_allowed(source: astra_core::SourcePolicy, local: bool) -> bool {
    match source {
        astra_core::SourcePolicy::LiveOnly => false,
        astra_core::SourcePolicy::LocalOnly => local,
        astra_core::SourcePolicy::CloudOnly => !local,
        _ => true,
    }
}

fn semantic_judgment_view(
    snapshot: &IntrospectSnapshot,
    request: &IntrospectRequest,
) -> Option<astra_services::semantic_judgment_observation::SemanticJudgmentView> {
    use astra_services::semantic_judgment_observation::{
        SemanticJudgmentCoverage, SemanticJudgmentView, semantic_judgment_facet_enabled,
    };
    if !semantic_judgment_facet_enabled(request.facet) {
        return None;
    }
    Some(
        if !judgment_source_allowed(request.source_policy, snapshot.semantic_judgments.as_ref().is_some_and(|view| view.scope == astra_services::semantic_judgment_observation::SemanticJudgmentScope::LocalJournalAtRead)) {
            SemanticJudgmentView::unavailable(SemanticJudgmentCoverage::SourceExcluded)
        } else {
            snapshot
                .semantic_judgments
                .clone()
                .unwrap_or_default()
                .bounded(request.depth)
        },
    )
}

fn judgment_usage_view(
    snapshot: &IntrospectSnapshot,
    request: &IntrospectRequest,
) -> Option<JudgmentUsageSnapshot> {
    if !matches!(
        request.facet,
        ObservationFacet::Session
            | ObservationFacet::Overview
            | ObservationFacet::Recent
            | ObservationFacet::Trace
    ) {
        return None;
    }
    Some(
        if !judgment_source_allowed(
            request.source_policy,
            snapshot
                .judgment_usage
                .as_ref()
                .is_some_and(|view| view.scope.is_local()),
        ) {
            JudgmentUsageSnapshot::unavailable(JudgmentUsageCoverage::SourceExcluded)
        } else {
            snapshot
                .judgment_usage
                .clone()
                .unwrap_or_default()
                .bounded(request.depth)
        },
    )
}

fn tool_result_judgment_view(
    snapshot: &IntrospectSnapshot,
    request: &IntrospectRequest,
) -> Option<astra_services::tool_result_selection_observation::ToolResultJudgmentView> {
    if !matches!(
        request.facet,
        ObservationFacet::Session
            | ObservationFacet::Overview
            | ObservationFacet::Recent
            | ObservationFacet::Trace
    ) {
        return None;
    }
    Some(if !judgment_source_allowed(request.source_policy, false) {
        astra_services::tool_result_selection_observation::ToolResultJudgmentView::unavailable(
            astra_services::tool_result_selection_observation::ToolResultJudgmentCoverage::SourceExcluded,
        )
    } else {
        snapshot.tool_result_judgments.clone().unwrap_or_default()
    })
}

/// Render a normalized request. Edge-only facets remain explicitly unavailable
/// when no local artifact provider intercepts them.
pub fn render_introspect_request(
    snapshot: &IntrospectSnapshot,
    request: &IntrospectRequest,
) -> String {
    let historical_horizon = matches!(
        request.horizon,
        ObservationHorizon::Turn | ObservationHorizon::Session | ObservationHorizon::CrossSession
    );
    if request.format.is_json() {
        return observation::render_introspect_report_json(snapshot, request);
    }

    let mut live_request = request.clone();
    if historical_horizon {
        live_request.horizon = ObservationHorizon::Recent;
    }
    let body = match live_request.facet {
        ObservationFacet::Session => {
            let depth = match live_request.depth {
                ObservationDepth::Hint => IntrospectTextDepth::Hint,
                ObservationDepth::Summary => IntrospectTextDepth::Summary,
                ObservationDepth::Diagnostic | ObservationDepth::Forensic => {
                    IntrospectTextDepth::Full
                }
            };
            render_introspect(snapshot, depth)
        }
        ObservationFacet::Recent | ObservationFacet::Trace => render_recent_rounds(snapshot),
        ObservationFacet::Volatile => render_volatile_pending(snapshot),
        ObservationFacet::Stall => render_stall_state(snapshot),
        ObservationFacet::Noise => render_injection_freshness(snapshot),
        ObservationFacet::Errors => render_errors(snapshot),
        ObservationFacet::Overview => render_all(snapshot),
        ObservationFacet::Cache | ObservationFacet::SessionMemory => {
            render_edge_local_unavailable(&live_request)
        }
    };
    let body = if let Some(usage) = judgment_usage_view(snapshot, request) {
        format!("{body}\n\n{}", usage.render())
    } else {
        body
    };
    let body = if let Some(semantics) = semantic_judgment_view(snapshot, request) {
        format!("{body}\n\n{}", semantics.render())
    } else {
        body
    };
    let body = if let Some(judgments) = tool_result_judgment_view(snapshot, request) {
        format!("{body}\n\n{}", judgments.render())
    } else {
        body
    };
    let evidence_revision = observation::evidence_revision(snapshot, &live_request);
    let boundary_prefix = format!(
        "## Observation Boundary\n\
snapshot_cutoff=before_current_introspect_execution; the selecting round may list `introspect` as requested/in-flight, and calls made after this snapshot are absent. Judgment usage, evaluation traces, and application receipts carry independent source scopes and capture/read cutoffs. Treat counts and states as scoped observations, not final session totals. evidence_revision={evidence_revision} covered_facets="
    );
    let candidate_facets = observation::text_covered_facets(&live_request).join(",");
    let candidate_boundary = format!("{boundary_prefix}{candidate_facets}");
    let candidate_output = if historical_horizon {
        format!(
            "## Introspect Live Projection\nrequested_horizon={} coverage=recent-only; use reflect for persisted causal evidence.\n\n{}\n\n{}",
            request.horizon.as_str(),
            candidate_boundary,
            body
        )
    } else {
        format!("{candidate_boundary}\n\n{body}")
    };
    // A text boundary is front-loaded and can outlive the body when the
    // downstream model sanitizer truncates the result. Only claim coverage
    // when the complete marked result fits that same model budget.
    let covered_facets = if candidate_output.chars().count()
        <= crate::tool::result::sanitize::INTROSPECT_MODEL_RESULT_CHARS
    {
        candidate_facets
    } else {
        String::new()
    };
    let boundary = format!("{boundary_prefix}{covered_facets}");
    if historical_horizon {
        format!(
            "## Introspect Live Projection\nrequested_horizon={} coverage=recent-only; use reflect for persisted causal evidence.\n\n{}\n\n{}",
            request.horizon.as_str(),
            boundary,
            body
        )
    } else {
        format!("{boundary}\n\n{body}")
    }
}

/// Extract the source-owned evidence revision from either the structured JSON
/// report or the bounded text projection.  Dynamic rendered counters are not
/// used as an identity fallback: if the marker is absent, the caller must
/// treat the delivered evidence as unmeasurable.
pub(crate) fn extract_evidence_revision(result: &str) -> Option<String> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(result) {
        if let Some(revision) = value
            .get("evidence_revision")
            .and_then(|value| value.as_str())
        {
            return valid_evidence_revision(revision).map(str::to_string);
        }
    }

    result.lines().find_map(|line| {
        let marker = "evidence_revision=";
        let start = line.find(marker)? + marker.len();
        let revision = line[start..].split_whitespace().next()?;
        valid_evidence_revision(revision).map(str::to_string)
    })
}

pub(crate) fn extract_covered_facets(result: &str) -> Option<Vec<String>> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(result) {
        if let Some(facets) = value
            .get("covered_facets")
            .and_then(|value| value.as_array())
        {
            if facets.iter().any(|facet| !facet.is_string()) {
                return None;
            }
            let parsed = facets
                .iter()
                .filter_map(|facet| facet.as_str())
                .map(str::to_string)
                .collect::<Vec<_>>();
            // An empty marker is meaningful: the report was bounded before
            // it could claim facet coverage.  Keep the distinction from a
            // missing marker so equality-only repeat detection can still
            // identify an identical delivery without treating it as useful
            // cross-facet coverage.
            return Some(parsed);
        }
    }

    result.lines().find_map(|line| {
        let marker = "covered_facets=";
        let start = line.find(marker)? + marker.len();
        let token = line[start..].split_whitespace().next().unwrap_or("");
        let facets = token
            .split(',')
            .filter(|facet| !facet.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        Some(facets)
    })
}

fn valid_evidence_revision(value: &str) -> Option<&str> {
    let (version, digest) = value.split_once(':')?;
    if version != "v1"
        || digest.len() != 64
        || !digest
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return None;
    }
    Some(value)
}

fn render_edge_local_unavailable(request: &IntrospectRequest) -> String {
    let reason = if request.source_policy.allows_edge_local_artifacts() {
        "no CLI/Edge-local artifact provider is attached"
    } else {
        "requested source_policy does not allow CLI/Edge-local artifacts"
    };
    format!(
        "## Introspect Unavailable\n\
         facet={} source_policy={}\n\
         CLI/Edge-local session artifacts are not visible from this runtime: {}.",
        request.facet.as_str(),
        request.source_policy.as_str(),
        reason,
    )
}

/// Render the introspect output at the requested text depth.
fn render_introspect(snapshot: &IntrospectSnapshot, depth: IntrospectTextDepth) -> String {
    match depth {
        IntrospectTextDepth::Hint => render_hint(snapshot),
        IntrospectTextDepth::Summary => render_summary(snapshot),
        IntrospectTextDepth::Full => render_full(snapshot),
    }
}

fn render_hint(s: &IntrospectSnapshot) -> String {
    let Some(frame) = s.runtime_feedback.as_ref() else {
        let mut out = format!(
            "runtime_feedback=not_yet_observed alerts={} providers={} tool_admission={}",
            s.alerts.len(),
            capacity_provider_inline_summary(s),
            tool_admission_inline_summary(s),
        );
        if s.snapshot_age_turns > 0 {
            out.push_str(&format!(" snapshot_age_turns={}", s.snapshot_age_turns));
        }
        return out;
    };
    let pressure = frame.context.token_pressure.map_or_else(
        || "unknown".to_string(),
        |value| format!("{:.0}%", value * 100.0),
    );
    let cache_share = prompt_cache_read_share_pct(s)
        .map_or_else(|| "unknown".to_string(), |value| format!("{value:.0}%"));
    let (input_total, cached_read, cache_create) = frame.run_usage.map_or_else(
        || ("unknown".into(), "unknown".into(), "unknown".into()),
        |usage| {
            (
                usage.total_input().to_string(),
                usage.cache_read.to_string(),
                usage.cache_creation.to_string(),
            )
        },
    );
    let mut out = format!(
        "pressure={} prompt_cache_read_share={} prompt_cache_scope=current_runtime_snapshot input_total={} cached_read={} cache_create={} turns={} alerts={} tier={}",
        pressure,
        cache_share,
        input_total,
        cached_read,
        cache_create,
        turn_budget_label(s),
        s.alerts.len(),
        format_args!("{:?}", frame.context.compaction_tier),
    );
    if let Some(eligible) = frame.context.estimated_cache_eligible_tokens {
        out.push_str(&format!(" prompt_cache_stable_prefix_tokens={eligible}"));
        if let Some(ratio) = frame.cache_read_vs_eligible_ratio() {
            out.push_str(&format!(" prompt_cache_read_over_stable_prefix={ratio:.2}"));
        }
    }
    if s.snapshot_age_turns > 0 {
        out.push_str(&format!(" snapshot_age_turns={}", s.snapshot_age_turns));
    }
    out.push_str(" model=");
    out.push_str(&frame.identity.model_id);
    out.push_str(" topology=");
    out.push_str(frame.identity.topology.as_str());
    out.push_str(" policy_feedback=");
    out.push_str(&frame.policy_feedback.inline_summary());
    if !s.capacity_provider_coverage.is_empty() {
        out.push_str(" providers=");
        out.push_str(&capacity_provider_inline_summary(s));
    }
    if !s.tool_admission.is_empty() {
        out.push_str(" tool_admission=");
        out.push_str(&tool_admission_inline_summary(s));
    }
    if !s.semantic_cache_decisions.is_empty() {
        out.push_str(&format!(
            " semantic_cache_decisions={}",
            s.semantic_cache_decisions.len()
        ));
    }
    if let Some(lifecycle) = s.invocation_lifecycle.as_ref() {
        out.push_str(&format!(
            " invocation_lifecycle=hot:{} prepared:{} dispatched:{} unknown:{} archives:{} refs:{}",
            lifecycle.hot_total,
            lifecycle.prepared,
            lifecycle.dispatched,
            lifecycle.outcome_unknown,
            lifecycle.archive_chunks,
            lifecycle.durable_artifact_references,
        ));
    }
    out
}

fn render_summary(s: &IntrospectSnapshot) -> String {
    let mut out = String::new();
    let Some(frame) = s.runtime_feedback.as_ref() else {
        out.push_str("## Current Runtime Snapshot\n");
        out.push_str("Runtime feedback: not yet observed. No provider round has been successfully ingested for this runtime.\n");
        if s.snapshot_age_turns > 0 {
            out.push_str(&format!("Snapshot age: {} turn(s)\n", s.snapshot_age_turns));
        }
        if !s.capacity_provider_coverage.is_empty() {
            out.push_str("Capacity providers: ");
            out.push_str(&capacity_provider_inline_summary(s));
            out.push('\n');
        }
        if !s.alerts.is_empty() {
            out.push_str("Alerts:\n");
            for alert in s.alerts.iter().take(3) {
                out.push_str("- ");
                out.push_str(alert);
                out.push('\n');
            }
        }
        if !s.working_memory_summary.is_empty() {
            out.push_str(&s.working_memory_summary);
            out.push('\n');
        }
        if !s.lifecycle_summary.is_empty() {
            out.push_str(&s.lifecycle_summary);
            out.push('\n');
        }
        if !s.tool_admission.is_empty() {
            out.push_str(&format!(
                "Route readiness: {}\n",
                tool_admission_inline_summary(s)
            ));
            if let Some(surface) = provider_tool_surface_inline_summary(s) {
                out.push_str(&format!("Provider wire tool surface: {surface}\n"));
            }
        }
        return out.trim_end().to_string();
    };
    let pressure = frame.context.token_pressure.map_or_else(
        || "unknown".to_string(),
        |value| format!("{:.0}%", value * 100.0),
    );
    let cache_share = prompt_cache_read_share_pct(s)
        .map_or_else(|| "unknown".to_string(), |value| format!("{value:.0}%"));
    let (input_total, fresh, cached_read, cache_create, output) = frame.run_usage.map_or_else(
        || {
            (
                "unknown".into(),
                "unknown".into(),
                "unknown".into(),
                "unknown".into(),
                "unknown".into(),
            )
        },
        |usage| {
            (
                usage.total_input().to_string(),
                usage.prompt.to_string(),
                usage.cache_read.to_string(),
                usage.cache_creation.to_string(),
                usage.completion.to_string(),
            )
        },
    );
    out.push_str(&format!(
        "## Current Runtime Snapshot\n\
         Scope: current live runtime snapshot; prompt-cache values are not durable session-wide aggregates.\n\
         Pressure: {} | Prompt cache read share: {} | Turns: {} | Tier: {}\n\
         Prompt tokens: input_total={} fresh={} cached_read={} cache_create={} | Output tokens: {}\n",
        pressure,
        cache_share,
        turn_budget_label(s),
        format_args!("{:?}", frame.context.compaction_tier),
        input_total,
        fresh,
        cached_read,
        cache_create,
        output,
    ));
    out.push_str("Current model: ");
    out.push_str(&frame.identity.model_id);
    out.push('\n');
    out.push_str("Execution topology: ");
    out.push_str(frame.identity.topology.as_str());
    out.push('\n');
    out.push_str("Runtime policy feedback: ");
    out.push_str(&frame.policy_feedback.inline_summary());
    out.push('\n');
    if s.snapshot_age_turns > 0 {
        out.push_str(&format!("Snapshot age: {} turn(s)\n", s.snapshot_age_turns));
    }
    if let Some(context_window_tokens) = frame.context.model_context_window_tokens {
        out.push_str(&format!(
            "Provider context window: {} tokens\n",
            context_window_tokens
        ));
    }
    if let Some(effective_input_limit_tokens) = frame.context.effective_input_limit_tokens {
        if let Some(estimated_input_tokens) = frame.context.estimated_input_tokens {
            out.push_str(&format!(
                "Effective input budget: {}/{} tokens ({:.0}% used)\n",
                estimated_input_tokens,
                effective_input_limit_tokens,
                (estimated_input_tokens as f64 / effective_input_limit_tokens as f64) * 100.0,
            ));
        } else {
            out.push_str(&format!(
                "Effective input budget: {} tokens (usage estimate unavailable)\n",
                effective_input_limit_tokens,
            ));
        }
    }
    if !s.capacity_provider_coverage.is_empty() {
        out.push_str("Capacity providers: ");
        out.push_str(&capacity_provider_inline_summary(s));
        out.push('\n');
    }
    if !s.alerts.is_empty() {
        out.push_str("Alerts:\n");
        for alert in s.alerts.iter().take(3) {
            out.push_str("- ");
            out.push_str(alert);
            out.push('\n');
        }
        if s.alerts.len() > 3 {
            out.push_str(&format!("  (+{} more)\n", s.alerts.len() - 3));
        }
    }
    if !s.working_memory_summary.is_empty() {
        out.push_str(&s.working_memory_summary);
        out.push('\n');
    }
    if !s.lifecycle_summary.is_empty() {
        out.push_str(&s.lifecycle_summary);
        out.push('\n');
    }
    if !s.tool_admission.is_empty() {
        out.push_str(&format!(
            "Route readiness: {}\n",
            tool_admission_inline_summary(s)
        ));
        if let Some(surface) = provider_tool_surface_inline_summary(s) {
            out.push_str(&format!("Provider wire tool surface: {surface}\n"));
        }
    }
    if !s.semantic_cache_decisions.is_empty() {
        out.push_str("Semantic read cache decisions: ");
        out.push_str(
            &s.semantic_cache_decisions
                .iter()
                .map(|entry| format!("{}={}", entry.tool_name, entry.state))
                .collect::<Vec<_>>()
                .join(", "),
        );
        out.push('\n');
    }
    if let Some(lifecycle) = s.invocation_lifecycle.as_ref() {
        out.push_str(&format!(
            "Durable invocation lifecycle: hot={} prepared={} dispatched={} succeeded={} failed={} rejected={} not_dispatched_rejections={} outcome_unknown={} archive_chunks={} artifact_refs={} reconciliations={} deferred={}\n",
            lifecycle.hot_total,
            lifecycle.prepared,
            lifecycle.dispatched,
            lifecycle.succeeded,
            lifecycle.failed,
            lifecycle.rejected,
            lifecycle.rejected_without_dispatch,
            lifecycle.outcome_unknown,
            lifecycle.archive_chunks,
            lifecycle.durable_artifact_references,
            lifecycle.reconciliation_events,
            lifecycle.compaction_deferred_events,
        ));
    }
    out.trim_end().to_string()
}

fn capacity_provider_inline_summary(s: &IntrospectSnapshot) -> String {
    capacity_provider_coverage_summary(&s.capacity_provider_coverage)
}

fn tool_admission_inline_summary(s: &IntrospectSnapshot) -> String {
    let total = s.tool_admission.len();
    let visible = s
        .tool_admission
        .iter()
        .filter(|entry| entry.visible)
        .count();
    let hidden = total.saturating_sub(visible);
    format!("visible={visible}/{total} hidden={hidden}")
}

fn provider_tool_surface_inline_summary(s: &IntrospectSnapshot) -> Option<String> {
    let observed = s
        .tool_admission
        .iter()
        .filter(|entry| entry.provider_visible.is_some())
        .count();
    (observed > 0).then(|| {
        let names = s
            .tool_admission
            .iter()
            .filter(|entry| entry.provider_visible == Some(true))
            .map(|entry| entry.tool_name.as_str())
            .collect::<Vec<_>>();
        format!("visible={} tools=[{}]", names.len(), names.join(", "))
    })
}

pub fn capacity_provider_coverage_summary(coverage: &[CapacityProviderCoverageEntry]) -> String {
    coverage
        .iter()
        .map(capacity_provider_coverage_entry_summary)
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn capacity_provider_coverage_entry_summary(
    provider: &CapacityProviderCoverageEntry,
) -> String {
    let mut label = format!("{}:{}", provider.provider_type, provider.status);
    if let Some(reason) = provider.unavailable_reason.as_deref() {
        label.push_str(" (");
        label.push_str(capacity_provider_unavailable_text(reason));
        label.push(')');
    }
    label
}

fn capacity_provider_unavailable_text(reason: &str) -> &str {
    match reason {
        "no_workspace_provider_bound" => "workspace executor not bound",
        "no_executor_provider_bound" => "executor provider not bound",
        "executor_offline" => "executor offline",
        "executor_status_unknown" => "executor status unknown",
        "no_request_scoped_mcp_provider_bound" => "request-scoped MCP provider not bound",
        "no_request_scoped_mcp_runtime_binding" => "request-scoped MCP runtime not bound",
        _ => "provider unavailable",
    }
}

pub fn turn_budget_label(s: &IntrospectSnapshot) -> String {
    let Some(frame) = s.runtime_feedback.as_ref() else {
        return "not_yet_observed".to_string();
    };
    let progress = frame.progress;
    let ceiling = progress
        .absolute_round_ceiling
        .map_or_else(|| "none".to_string(), |value| value.to_string());
    format!(
        "session_turn={} llm_rounds_completed={} slice_boundary={} slice_remaining={} absolute_round_ceiling={}",
        progress.session_turn,
        progress.llm_rounds_completed,
        progress.slice_round_limit,
        progress.slice_rounds_remaining,
        ceiling,
    )
}

pub fn mark_snapshot_age(snapshot: &mut IntrospectSnapshot, current_session_turn: u32) {
    if let Some(frame) = snapshot.runtime_feedback.as_ref() {
        let age = current_session_turn.saturating_sub(frame.progress.session_turn);
        snapshot.snapshot_age_turns = snapshot.snapshot_age_turns.max(age);
    }
}

pub fn prompt_cache_read_share_pct(s: &IntrospectSnapshot) -> Option<f64> {
    s.runtime_feedback
        .as_ref()
        .and_then(|frame| frame.cache_hit_ratio().map(|ratio| ratio * 100.0))
}

pub fn prompt_cache_fresh_input_tokens(s: &IntrospectSnapshot) -> Option<u64> {
    s.runtime_feedback
        .as_ref()
        .and_then(|frame| frame.run_usage.map(|usage| usage.prompt))
}

/// Estimated provider-visible stable prefix size for diagnostics. This is
/// intentionally separate from `prompt_cache_read_share_pct`: providers may
/// cache conversation history beyond the stable system/tool prefix.
pub fn prompt_cache_stable_prefix_tokens(s: &IntrospectSnapshot) -> Option<u64> {
    s.runtime_feedback
        .as_ref()
        .and_then(|frame| frame.context.estimated_cache_eligible_tokens)
}

fn render_full(s: &IntrospectSnapshot) -> String {
    let mut out = render_summary(s);
    out.push('\n');

    if !s.tool_health.is_empty() {
        out.push_str("\n## Tool Health\n");
        out.push_str(
            "| Tool | Calls | Errors | Input rejects | Avg ms | ConsecFail | Avoid | LastFail |\n",
        );
        out.push_str(
            "|------|-------|--------|---------------|--------|------------|-------|----------|\n",
        );
        for t in &s.tool_health {
            let avoidance = if t.avoidance_advised { "YES" } else { "-" };
            let last_fail = t.last_failure_category.as_deref().unwrap_or("-");
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
                t.name,
                t.calls,
                t.errors,
                t.input_validation_failures,
                t.avg_ms,
                t.consecutive_failures,
                avoidance,
                last_fail
            ));
        }
    }

    if !s.step_latency.is_empty() {
        out.push('\n');
        out.push_str(&render_step_latency(s));
    }

    if !s.semantic_cache_decisions.is_empty() {
        out.push_str("\n## Semantic Read Cache Decisions\n");
        for decision in &s.semantic_cache_decisions {
            out.push_str("- ");
            out.push_str(&decision.tool_name);
            out.push_str(": ");
            out.push_str(&decision.state);
            if let Some(key_id) = decision.key_id.as_deref() {
                out.push_str(" key=");
                out.push_str(key_id);
            }
            out.push('\n');
        }
    }

    if let Some(lifecycle) = s.invocation_lifecycle.as_ref() {
        out.push_str("\n## Durable Invocation Lifecycle\n");
        if let Some(run_id) = lifecycle.run_id.as_deref() {
            out.push_str(&format!("Run: {run_id}\n"));
        } else {
            out.push_str("Scope: session\n");
        }
        out.push_str(&format!(
            "Hot: total={} prepared={} dispatched={} succeeded={} failed={} rejected={} outcome_unknown={}\n",
            lifecycle.hot_total,
            lifecycle.prepared,
            lifecycle.dispatched,
            lifecycle.succeeded,
            lifecycle.failed,
            lifecycle.rejected,
            lifecycle.outcome_unknown,
        ));
        out.push_str(&format!(
            "Evidence: not_dispatched_rejections={} archive_chunks={} artifact_refs={} reconciliation_events={} deferred_events={}\n",
            lifecycle.rejected_without_dispatch,
            lifecycle.archive_chunks,
            lifecycle.durable_artifact_references,
            lifecycle.reconciliation_events,
            lifecycle.compaction_deferred_events,
        ));
        if let Some(generation) = lifecycle.compaction_cursor_generation {
            out.push_str(&format!(
                "Compaction cursor: generation={} updated_at={}\n",
                generation,
                lifecycle
                    .compaction_cursor_updated_at
                    .as_deref()
                    .unwrap_or("unknown"),
            ));
        }
    }

    if s.alerts.len() > 3 {
        out.push_str("\n## All Alerts\n");
        for alert in &s.alerts {
            out.push_str("- ");
            out.push_str(alert);
            out.push('\n');
        }
    }

    out.trim_end().to_string()
}

/// Render `facet=recent` — the in-memory ring of recent LLM rounds.
/// Compact table per round with tokens + tool counts + timing.
pub fn render_recent_rounds(s: &IntrospectSnapshot) -> String {
    if s.recent_rounds.is_empty() {
        return "## Recent Rounds\n(No rounds recorded yet in this turn.)".to_string();
    }
    let mut out = String::from(
        "## Recent Rounds (most recent last)\n\
         | t_r | provider | in | cached | cc_w | out | tools | dur_ms | finish |\n\
         |-----|----------|----|--------|------|-----|-------|--------|--------|\n",
    );
    for r in &s.recent_rounds {
        let provider = if r.provider.is_empty() {
            "-"
        } else {
            r.provider.as_str()
        };
        let tools_label = if r.tool_call_names.is_empty() {
            "-".to_string()
        } else {
            r.tool_call_names.join(",")
        };
        let finish = r.finish_reason.as_deref().unwrap_or("-");
        out.push_str(&format!(
            "| t{}_r{} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            r.turn,
            r.round,
            provider,
            r.prompt_tokens,
            r.cache_read_tokens,
            r.cache_creation_tokens,
            r.completion_tokens,
            tools_label,
            r.duration_ms,
            finish,
        ));
    }
    // Summary stats so the LLM gets a one-liner without re-totalling.
    let total_fresh: u64 = s.recent_rounds.iter().map(|r| r.prompt_tokens).sum();
    let total_cached: u64 = s.recent_rounds.iter().map(|r| r.cache_read_tokens).sum();
    let total_created: u64 = s
        .recent_rounds
        .iter()
        .map(|r| r.cache_creation_tokens)
        .sum();
    let input_total: u64 = total_fresh
        .saturating_add(total_cached)
        .saturating_add(total_created);
    let pct = if input_total > 0 {
        (total_cached as f64 / input_total as f64) * 100.0
    } else {
        0.0
    };
    out.push_str(&format!(
        "\nRing: {} rounds, prompt_cache_read_share={:.0}% (cached_read={}/input_total={}, cache_create={}).\n",
        s.recent_rounds.len(),
        pct,
        total_cached,
        input_total,
        total_created,
    ));
    out
}

/// Render per-step attribution for model wait vs tools.
pub fn render_step_latency(s: &IntrospectSnapshot) -> String {
    if s.step_latency.is_empty() {
        return "## Step Latency\n(No step latency data recorded yet.)".to_string();
    }

    let mut out = String::from(
        "## Step Latency (model wait vs tools)\n\
         | step | total | pre_tool | tool_ms | max_tool | calls | skipped | dominant | first_tool | terminal |\n\
         |------|-------|----------|---------|----------|-------|---------|----------|------------|----------|\n",
    );

    let mut rows: Vec<&StepLatencySnapshotEntry> = s.step_latency.iter().rev().take(12).collect();
    rows.reverse();
    for entry in rows {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            entry.step_id,
            fmt_opt_ms(entry.total_ms),
            fmt_opt_ms(entry.pre_tool_wait_ms),
            entry.tool_execution_ms,
            entry.max_tool_execution_ms,
            entry.tool_call_count,
            entry.skipped_tool_count,
            entry.dominant_phase,
            entry.first_tool_name.as_deref().unwrap_or("-"),
            entry.terminal_event_kind.as_deref().unwrap_or("-"),
        ));
    }

    if let Some(last) = s.step_latency.last() {
        out.push_str(&format!(
            "\nLatest: dominant={} pre_tool={} tool_ms={} first_tool={}\n",
            last.dominant_phase,
            fmt_opt_ms(last.pre_tool_wait_ms),
            last.tool_execution_ms,
            last.first_tool_name.as_deref().unwrap_or("-"),
        ));
    }
    out
}

fn fmt_opt_ms(value: Option<u64>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".to_string())
}

/// Render `facet=volatile` — what's queued in the volatile lane right now
/// (about to ride the next LLM call's preamble).
pub fn render_volatile_pending(s: &IntrospectSnapshot) -> String {
    if s.volatile_pending.is_empty() {
        return "## Volatile Lane\n(Empty — no pending runtime injections.)".to_string();
    }
    let mut out = String::from("## Volatile Lane (pending for next LLM call)\n");
    for (i, inj) in s.volatile_pending.iter().enumerate() {
        out.push_str(&format!(
            "- [{i}] **{kind}** (round {round}) — {preview}\n",
            i = i,
            kind = inj.kind,
            round = inj.round_index,
            preview = preview_line(&inj.content, 120),
        ));
    }
    out
}

/// Render `facet=stall` — stall / loop-guard telemetry.
pub fn render_stall_state(s: &IntrospectSnapshot) -> String {
    let st = &s.stall_state;
    let any_advisory = !st.advisory_signals.is_empty();
    if st.events.is_empty()
        && st.introspection_count == 0
        && !any_advisory
        && s.circuit_breaker.is_none()
    {
        return "## Stall / Loop-Guard\n(No events or advisory signals recorded this turn.)"
            .to_string();
    }
    let mut out = String::from("## Stall / Loop-Guard\n");
    out.push_str(&format!(
        "Circuit-breaker introspections: {}\n",
        st.introspection_count,
    ));
    if !st.events.is_empty() {
        out.push_str("\n### Recent stall events\n");
        for e in &st.events {
            out.push_str("- ");
            out.push_str(e);
            out.push('\n');
        }
    }
    if any_advisory {
        out.push_str("\n### Advisory signals emitted this turn\n");
        for correction in &st.advisory_signals {
            out.push_str("- ");
            out.push_str(correction);
            out.push('\n');
        }
    }
    if let Some(cb) = &s.circuit_breaker {
        out.push_str(&format!(
            "\n### Bridge Circuit Breaker\nstate={} failures={} successes={} consecutive_failures={}\n",
            cb.state, cb.failure_count, cb.success_count, cb.consecutive_failures,
        ));
    }
    out
}

/// Render `facet=errors` — recent tool failures with error previews.
pub fn render_errors(s: &IntrospectSnapshot) -> String {
    if s.tool_errors.is_empty() {
        return "## Recent Tool Errors\n(No failures in this live runtime projection. Admission rejections and durable session alerts may exist outside this recent-tool view; use reflect for session-wide evidence.)".to_string();
    }
    let mut out = String::from(
        "## Recent Tool Errors (newest first)\n\
         | Tool | Category | Turn | Age(s) | Preview |\n\
         |------|----------|------|--------|---------|\n",
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for e in &s.tool_errors {
        let age = now.saturating_sub(e.at_epoch);
        let cat = e.failure_category.as_deref().unwrap_or("-");
        let preview = e
            .error_preview
            .as_deref()
            .map(|p| p.replace('|', "\\|").replace('\n', " "))
            .unwrap_or_else(|| "-".to_string());
        let short: String = preview.chars().take(80).collect();
        let turn_str = if e.turn > 0 {
            format!("T{}R{}", e.turn, e.round)
        } else {
            "-".to_string()
        };
        out.push_str(&format!(
            "| {} | {} | {} | {}s | {} |\n",
            e.tool, cat, turn_str, age, short,
        ));
        // Detail line for file errors
        if let Some(ref path) = e.file_path {
            let range = e.file_range.as_deref().unwrap_or("");
            let msg = if e.error_message.is_empty() {
                "-"
            } else {
                &e.error_message
            };
            let msg_short: String = msg.chars().take(60).collect();
            out.push_str(&format!(
                "  → path: `{}{}` — {}\n",
                path,
                if range.is_empty() {
                    String::new()
                } else {
                    format!(":{range}")
                },
                msg_short.replace('\n', " "),
            ));
        } else if !e.error_message.is_empty() {
            let msg_short: String = e.error_message.chars().take(100).collect();
            out.push_str(&format!("  → {}\n", msg_short.replace('\n', " "),));
        }
    }
    if !s.tool_errors.is_empty() {
        out.push_str("\nSignature hints:\n");
        for e in s.tool_errors.iter().take(5) {
            if !e.signature_hint.is_empty() {
                out.push_str(&format!("- {}: {}\n", e.tool, e.signature_hint));
            }
        }
    }
    out
}

/// Render `facet=noise` — per-channel freshness of runtime-injected
/// prompt signals. Surfaces stale injections (e.g., a "Recent test
/// failures" entry that has been re-rendered unchanged for 58 rounds
/// — session f85a02bb). Operators and the model can use this to
/// distinguish fresh runtime context from signals that have aged out.
pub fn render_injection_freshness(s: &IntrospectSnapshot) -> String {
    if s.injection_freshness.is_empty() {
        return "## Injection Freshness\n(No injections tracked yet — runtime has not observed any round.)".to_string();
    }
    let mut out = String::from(&format!(
        "## Injection Freshness (round {})\n\
         | channel | status | first_seen | rounds_alive | preview |\n\
         |---------|--------|------------|--------------|---------|\n",
        s.current_round,
    ));
    let mut stale_count = 0usize;
    let mut tracked_count = 0usize;
    for entry in &s.injection_freshness {
        let (status_label, rounds_alive_str, first_seen_str) = match &entry.status {
            ChannelStatus::Untracked => ("untracked", "-".to_string(), "-".to_string()),
            ChannelStatus::Empty { first_seen_round } => {
                ("empty", "-".to_string(), format!("r{first_seen_round}"))
            }
            ChannelStatus::Fresh { rounds_alive } => {
                tracked_count += 1;
                (
                    "fresh",
                    rounds_alive.to_string(),
                    entry
                        .first_seen_round
                        .map(|r| format!("r{r}"))
                        .unwrap_or_else(|| "-".to_string()),
                )
            }
            ChannelStatus::Stale { rounds_alive } => {
                tracked_count += 1;
                stale_count += 1;
                (
                    "⚠ STALE",
                    rounds_alive.to_string(),
                    entry
                        .first_seen_round
                        .map(|r| format!("r{r}"))
                        .unwrap_or_else(|| "-".to_string()),
                )
            }
        };
        let preview = if entry.preview.is_empty() {
            "-".to_string()
        } else {
            entry.preview.replace('|', "\\|")
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            channel_tag(entry.channel),
            status_label,
            first_seen_str,
            rounds_alive_str,
            preview,
        ));
    }
    out.push('\n');
    if tracked_count == 0 {
        out.push_str(
            "Summary: no channel has any non-empty content observed — runtime has not injected anything.\n",
        );
    } else if stale_count == 0 {
        out.push_str(&format!(
            "Summary: {tracked_count}/{tracked_count} tracked channels are fresh.\n",
        ));
    } else {
        out.push_str(&format!(
            "Summary: {stale_count}/{tracked_count} channels unchanged beyond stale threshold — these signals may no longer reflect current state; verify before acting on them.\n",
        ));
        let advisories =
            crate::injection_tracking::stale_channel_advisories(&s.injection_freshness);
        if !advisories.is_empty() {
            out.push_str("Advisories:\n");
            for advisory in advisories.iter().take(5) {
                out.push_str("- ");
                out.push_str(advisory);
                out.push('\n');
            }
        }
    }
    out
}

fn channel_tag(ch: InjectionChannel) -> &'static str {
    ch.tag()
}

/// Render `facet=all` — everything. Useful when debugging / when
/// the agent isn't sure which lens to pick. Same content as
/// `render_full` + recent/volatile/stall facets + injection freshness + errors.
pub fn render_all(s: &IntrospectSnapshot) -> String {
    let mut out = render_full(s);
    out.push_str("\n\n");
    out.push_str(&render_recent_rounds(s));
    out.push_str("\n\n");
    out.push_str(&render_volatile_pending(s));
    out.push_str("\n\n");
    out.push_str(&render_stall_state(s));
    out.push_str("\n\n");
    out.push_str(&render_injection_freshness(s));
    out.push_str("\n\n");
    out.push_str(&render_errors(s));
    out
}

fn preview_line(text: &str, max: usize) -> String {
    let first_line = text.lines().next().unwrap_or("");
    let first_line_trimmed: String = first_line.trim().chars().take(max).collect();
    if first_line_trimmed.len() < first_line.trim().len() {
        format!("{first_line_trimmed}…")
    } else {
        first_line_trimmed
    }
}

#[cfg(test)]
pub(crate) fn test_runtime_feedback(
    session_turn: u32,
    llm_rounds_completed: u32,
    slice_rounds_remaining: u32,
) -> crate::context_feedback::RuntimeFeedbackFrame {
    crate::context_feedback::RuntimeFeedbackFrame {
        schema_version: crate::context_feedback::RuntimeFeedbackFrame::SCHEMA_VERSION,
        identity: crate::context_feedback::RuntimeFeedbackIdentity {
            session_id: "session-1".into(),
            run_id: "run-1".into(),
            agent_id: "agent-1".into(),
            model_id: "deepseek-v4-flash".into(),
            topology: astra_services::ModelRequestTopology::ServerOnly,
            request: None,
        },
        progress: crate::context_feedback::RuntimeFeedbackProgress {
            session_turn,
            agentic_round_index: llm_rounds_completed.saturating_sub(1),
            llm_rounds_completed,
            slice_round_limit: llm_rounds_completed.saturating_add(slice_rounds_remaining),
            slice_rounds_remaining,
            absolute_round_ceiling: None,
        },
        context: crate::context_feedback::RuntimeContextFeedback {
            prompt_cache_identity: None,
            model_context_window_tokens: Some(1_000_000),
            effective_input_limit_tokens: Some(800_000),
            estimated_input_tokens: Some(132_000),
            estimated_cache_eligible_tokens: Some(95_000),
            token_pressure: Some(0.72),
            compaction_tier: crate::compaction_types::CompactionTier::Normal,
        },
        request_usage: Some(crate::token_accounting::TokenAccounting::from_fields(
            42_000, 95_000, 8_000, 12_000,
        )),
        run_usage: Some(crate::token_accounting::TokenAccounting::from_fields(
            42_000, 95_000, 8_000, 12_000,
        )),
        was_truncated: false,
        cache_break_detected: None,
        policy_feedback: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn evidence_revision_parser_accepts_json_and_text_but_fails_closed() {
        let revision = "v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let json = serde_json::json!({"evidence_revision": revision}).to_string();
        assert_eq!(extract_evidence_revision(&json).as_deref(), Some(revision));
        assert_eq!(
            extract_evidence_revision(&format!(
                "## Observation Boundary\nevidence_revision={revision}"
            ))
            .as_deref(),
            Some(revision)
        );
        assert!(extract_evidence_revision("evidence_revision=v1:short").is_none());
        assert!(extract_evidence_revision("dynamic counters only").is_none());
    }

    #[test]
    fn covered_facets_parser_accepts_structured_and_text_boundaries() {
        let json = serde_json::json!({
            "covered_facets": ["session", "recent", "errors"]
        })
        .to_string();
        assert_eq!(
            extract_covered_facets(&json),
            Some(vec![
                "session".to_string(),
                "recent".to_string(),
                "errors".to_string()
            ])
        );
        assert_eq!(
            extract_covered_facets(
                "## Observation Boundary\nevidence_revision=v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa covered_facets=session,recent"
            ),
            Some(vec!["session".to_string(), "recent".to_string()])
        );
        assert_eq!(extract_covered_facets("covered_facets="), Some(Vec::new()));
        assert_eq!(
            extract_covered_facets(r#"{"covered_facets":["overview", 1]}"#),
            None
        );
    }

    #[test]
    fn semantic_judgment_detail_has_one_bounded_report_owner() {
        use astra_services::semantic_judgment_observation::*;
        use astra_turn_types::*;
        let observation = SemanticJudgmentObservationV1 {
            schema_version: 1,
            correlation: SemanticJudgmentCorrelationV1 {
                run_id: "run-1".into(),
                turn: 1,
                round: 2,
                owner_generation: None,
                evaluation_span_id: "eval-1".into(),
                invocation: SemanticJudgmentInvocationV1::Unavailable,
            },
            fact: SemanticJudgmentFactV1 {
                stage: RequestJudgmentStageV1::Initial,
                result: RequestJudgmentResultV1::NotDispatched {
                    reason: SemanticJudgmentPreDispatchReasonV1::NoOffering,
                },
            },
        };
        let capture = SemanticJudgmentCapture {
            available: true,
            capture_incomplete: true,
            truncated: true,
            candidates_scanned: 12,
            duplicate_observations: 0,
            omitted_observations: 2,
            observations: (0..10)
                .map(|i| SemanticJudgmentTraceObservation {
                    observation_span_id: format!("span-{i}"),
                    observation: observation.clone(),
                })
                .collect(),
            gaps: vec![
                SemanticJudgmentCaptureGap::TraceMayBeDropped,
                SemanticJudgmentCaptureGap::ObservationLimit,
            ],
        };
        let snapshot = IntrospectSnapshot {
            semantic_judgments: Some(SemanticJudgmentView::from_capture(capture)),
            ..Default::default()
        };
        let request = IntrospectRequest::from_args(&serde_json::json!({"depth":"hint"}));
        let report = build_introspect_report(&snapshot, &request);
        let semantics = report.semantic_judgments.as_ref().unwrap();
        assert_eq!(semantics.observations.len(), 2);
        assert_eq!(semantics.omitted_details, 8);
        assert_eq!(semantics.capture_omitted_observations, 2);
        assert_eq!(semantics.counts.as_ref().unwrap().not_dispatched, 10);
        assert!(!report.summary.contains("no_offering"));
        assert!(
            report
                .evidence
                .iter()
                .all(|e| !e.summary.contains("no_offering"))
        );
        assert!(
            report
                .observations
                .iter()
                .all(|o| !o.summary.contains("no_offering"))
        );
        // One reason per retained fact, never replicated through prose/graph.
        assert_eq!(
            serde_json::to_string(&report)
                .unwrap()
                .matches("no_offering")
                .count(),
            2
        );
    }

    #[test]
    fn semantic_judgment_consumer_preserves_coverage_and_source_boundaries() {
        use astra_services::semantic_judgment_observation::{
            SemanticJudgmentCoverage as Coverage, SemanticJudgmentView,
        };
        let snapshot = IntrospectSnapshot {
            semantic_judgments: Some(SemanticJudgmentView::unavailable(Coverage::QueryFailed)),
            ..Default::default()
        };
        for (policy, expected) in [
            ("auto", Coverage::QueryFailed),
            ("live_only", Coverage::SourceExcluded),
            ("local_only", Coverage::SourceExcluded),
        ] {
            let request =
                IntrospectRequest::from_args(&serde_json::json!({"source_policy":policy}));
            let report = build_introspect_report(&snapshot, &request);
            let view = report.semantic_judgments.as_ref().unwrap();
            assert_eq!(view.coverage, expected);
            assert!(view.counts.is_none());
            assert!(view.capture_incomplete);
            assert!(render_introspect_request(&snapshot, &request).contains(&view.render()));
            assert!(
                report
                    .observations
                    .iter()
                    .all(|o| o.kind != "semantic_judgment_trace")
            );
            assert_eq!(
                report.data_coverage.providers["semantic_judgment_trace"].status,
                "missing"
            );
        }
        let request = IntrospectRequest::from_args(&serde_json::json!({"facet":"errors"}));
        assert!(
            build_introspect_report(&snapshot, &request)
                .semantic_judgments
                .is_none()
        );
        assert!(!render_introspect_request(&snapshot, &request).contains("Semantic judgments:"));
    }

    #[test]
    fn tool_result_judgment_is_a_shared_typed_user_facing_fact() {
        use astra_services::tool_result_selection_observation::{
            ToolResultApplicationCounts, ToolResultJudgmentCoverage, ToolResultJudgmentModel,
            ToolResultJudgmentView,
        };
        let snapshot = IntrospectSnapshot {
            tool_result_judgments: Some(ToolResultJudgmentView {
                evaluation_coverage: ToolResultJudgmentCoverage::Available,
                application_coverage: ToolResultJudgmentCoverage::Available,
                evaluations: 1,
                selected: 1,
                applications: ToolResultApplicationCounts {
                    included: 1,
                    ..Default::default()
                },
                models: vec![ToolResultJudgmentModel {
                    provider: "jet".into(),
                    model: "jev-1.13.0".into(),
                    observed_invocations: 1,
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let request = IntrospectRequest::from_args(&serde_json::json!({"format":"json"}));
        let report = build_introspect_report(&snapshot, &request);
        assert_eq!(report.tool_result_judgments, snapshot.tool_result_judgments);
        let observation = report
            .observations
            .iter()
            .find(|item| item.kind == "tool_result_judgment")
            .expect("tool-result judgment observation");
        assert!(observation.summary.contains("jev-1.13.0 via jet"));
        assert!(observation.summary.contains("1 included"));
        let text_request = IntrospectRequest::from_args(&serde_json::json!({}));
        let text = render_introspect_request(&snapshot, &text_request);
        assert!(text.contains("Tool-result judgment:"));
        assert!(text.contains("jev-1.13.0 via jet"));

        let receipt_only = IntrospectSnapshot {
            tool_result_judgments: Some(ToolResultJudgmentView {
                evaluation_coverage: ToolResultJudgmentCoverage::NotObserved,
                application_coverage: ToolResultJudgmentCoverage::Available,
                applications: ToolResultApplicationCounts {
                    included: 1,
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(
            build_introspect_report(&receipt_only, &request)
                .observations
                .iter()
                .any(|item| item.kind == "tool_result_judgment")
        );

        let excluded = IntrospectRequest::from_args(
            &serde_json::json!({"format":"json", "source_policy":"local_only"}),
        );
        assert_eq!(
            build_introspect_report(&snapshot, &excluded)
                .tool_result_judgments
                .unwrap()
                .evaluation_coverage,
            ToolResultJudgmentCoverage::SourceExcluded
        );
    }

    fn judgment_facts(count: usize) -> astra_turn_types::ExplainAnalyzeAuxiliaryUsageV1 {
        use astra_turn_types::*;
        ExplainAnalyzeAuxiliaryUsageV1 {
            available: true,
            truncated: false,
            attempts: (0..count)
                .map(|index| ExplainAnalyzeAuxiliaryAttemptV1 {
                    attempt_id: format!("attempt-{index}"),
                    provider: "actual-provider".into(),
                    offering_id: "actual-offering".into(),
                    model_name: "actual-model".into(),
                    purpose: "verification_judge".into(),
                    operation_id: "request_judgment".into(),
                    usage_status: if index == 0 {
                        ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderPartial
                    } else {
                        ExplainAnalyzeAuxiliaryUsageStatusV1::Unavailable
                    },
                    usage: (index == 0).then_some(ExplainAnalyzeTokenUsageV1 {
                        basis: ExplainAnalyzeUsageBasisV1::ProviderPartial,
                        fresh_input_tokens: Some(10),
                        cache_read_tokens: Some(20),
                        cache_creation_tokens: None,
                        output_tokens: Some(5),
                    }),
                })
                .collect(),
        }
    }

    #[test]
    fn judgment_usage_preserves_physical_identity_missing_buckets_and_bounded_detail() {
        let usage = JudgmentUsageSnapshot::from_ledger(judgment_facts(40));
        assert_eq!(usage.observed_attempts, Some(40));
        assert_eq!(usage.attempts_without_complete_usage, Some(40));
        assert_eq!(usage.attempts.len(), 32);
        let hint = usage.bounded(ObservationDepth::Hint);
        assert_eq!(hint.attempts.len(), 2);
        assert_eq!(hint.omitted_attempts, 38);
        let text = hint.render();
        assert!(text.contains("provider=actual-provider offering=actual-offering model=actual-model operation=request_judgment"));
        assert!(text.contains("input=at_least_30 (incomplete) output=5"));
        assert!(text.contains("input=unknown output=unknown"));
        let snapshot = IntrospectSnapshot {
            judgment_usage: Some(usage),
            ..Default::default()
        };
        let request =
            IntrospectRequest::from_args(&serde_json::json!({"depth":"hint", "format":"json"}));
        let report = build_introspect_report(&snapshot, &request);
        assert_eq!(report.judgment_usage.as_ref().unwrap().omitted_attempts, 38);
        assert_eq!(report.data_coverage.overall, "partial");
        assert!(
            report
                .evidence
                .iter()
                .any(|e| e.summary.contains("actual-offering"))
        );
        let wire = serde_json::to_value(report).unwrap();
        assert_eq!(
            wire["judgment_usage"]["scope"],
            "session_supported_judgment_operations_at_ledger_read"
        );
        assert!(wire["judgment_usage"]["attempts"][1].get("usage").is_none());
    }

    #[test]
    fn judgment_usage_groups_separate_providers_and_include_hidden_retries() {
        let mut facts = judgment_facts(40);
        for attempt in &mut facts.attempts {
            attempt.provider = "jev".into();
        }
        facts.attempts[1].provider = "deepseek".into();
        let retry_usage = facts.attempts[0].usage.clone();
        facts.attempts[39].usage = retry_usage;
        facts.attempts[39].usage_status =
            astra_turn_types::ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderPartial;
        let usage = JudgmentUsageSnapshot::from_ledger(facts).bounded(ObservationDepth::Hint);
        assert_eq!(usage.attempts.len(), 2);
        assert_eq!(usage.groups.len(), 2);
        let jev = usage.groups.iter().find(|g| g.provider == "jev").unwrap();
        assert_eq!(jev.attempts, 39);
        assert_eq!(jev.known_input_tokens, 60);
        assert_eq!(jev.known_output_tokens, 10);
        assert!(!jev.input_complete && !jev.output_complete);
        let deepseek = usage
            .groups
            .iter()
            .find(|g| g.provider == "deepseek")
            .unwrap();
        assert_eq!(deepseek.attempts, 1);
        assert_eq!(deepseek.known_input_tokens, 0);
        assert!(!deepseek.input_complete);
        assert!(usage.render().contains("group provider=jev"));
        let mut facts = judgment_facts(4);
        facts.attempts[1].offering_id = "different-offering".into();
        facts.attempts[2].model_name = "different-model".into();
        facts.attempts[3].operation_id = "different-operation".into();
        let usage = JudgmentUsageSnapshot::from_ledger(facts);
        assert_eq!(
            usage.groups.len(),
            4,
            "every identity dimension is preserved"
        );
        let hint = usage.bounded(ObservationDepth::Hint);
        assert_eq!(hint.groups.len(), 2);
        assert_eq!(hint.omitted_groups, 2);
        assert_eq!(hint.known_input_tokens, Some(30));
    }

    #[test]
    fn judgment_usage_exact_zero_is_distinct_from_missing_usage() {
        let mut facts = judgment_facts(1);
        let attempt = &mut facts.attempts[0];
        attempt.usage_status =
            astra_turn_types::ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderExact;
        let usage = attempt.usage.as_mut().unwrap();
        usage.basis = astra_turn_types::ExplainAnalyzeUsageBasisV1::ProviderExact;
        usage.fresh_input_tokens = Some(0);
        usage.cache_read_tokens = Some(0);
        usage.cache_creation_tokens = Some(0);
        usage.output_tokens = Some(0);
        let snapshot = JudgmentUsageSnapshot::from_ledger(facts);
        assert_eq!(snapshot.attempts_without_complete_usage, Some(0));
        assert_eq!(snapshot.known_input_tokens, Some(0));
        assert_eq!(snapshot.known_output_tokens, Some(0));
        assert!(snapshot.input_complete && snapshot.output_complete);
        assert!(snapshot.render().contains("input=0 output=0"));
    }

    #[test]
    fn judgment_usage_aggregates_all_attempts_before_display_limits() {
        let mut facts = judgment_facts(40);
        // The only complete usage is outside even the forensic detail window.
        let last = facts.attempts.last_mut().unwrap();
        last.usage_status = astra_turn_types::ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderExact;
        last.usage = Some(astra_turn_types::ExplainAnalyzeTokenUsageV1 {
            basis: astra_turn_types::ExplainAnalyzeUsageBasisV1::ProviderExact,
            fresh_input_tokens: Some(100),
            cache_read_tokens: Some(200),
            cache_creation_tokens: Some(300),
            output_tokens: Some(70),
        });
        let snapshot = JudgmentUsageSnapshot::from_ledger(facts).bounded(ObservationDepth::Hint);
        assert_eq!(snapshot.known_input_tokens, Some(630));
        assert_eq!(snapshot.known_output_tokens, Some(75));
        assert!(!snapshot.input_complete && !snapshot.output_complete);
        assert_eq!(snapshot.attempts.len(), 2);
        assert_eq!(snapshot.omitted_attempts, 38);
        let report = build_introspect_report(
            &IntrospectSnapshot {
                judgment_usage: Some(snapshot.clone()),
                ..Default::default()
            },
            &IntrospectRequest::from_args(&serde_json::json!({"depth":"hint"})),
        );
        let wire = serde_json::to_value(report).unwrap();
        assert_eq!(wire["judgment_usage"]["known_input_tokens"], 630);
        assert_eq!(wire["judgment_usage"]["known_output_tokens"], 75);
        assert_eq!(wire["judgment_usage"]["input_complete"], false);
        assert!(snapshot.render().contains(
            "total_input=at_least_630 (incomplete) total_output=at_least_75 (incomplete)"
        ));
        let mut partial = judgment_facts(1);
        let usage = JudgmentUsageSnapshot::from_ledger(partial.clone());
        assert!(!usage.input_complete, "unknown cache creation is not zero");
        assert!(
            usage.output_complete,
            "known output remains complete independently"
        );
        partial.attempts[0].usage = None;
        let unavailable_usage = JudgmentUsageSnapshot::from_ledger(partial);
        assert_eq!(unavailable_usage.known_input_tokens, Some(0));
        assert!(!unavailable_usage.input_complete);
        let unavailable_ledger = JudgmentUsageSnapshot::unavailable(JudgmentUsageCoverage::NoPool);
        assert_eq!(unavailable_ledger.known_input_tokens, None);
        assert!(!unavailable_ledger.input_complete && !unavailable_ledger.output_complete);
    }

    #[test]
    fn judgment_usage_source_exclusion_and_text_json_scope_agree() {
        let snapshot = IntrospectSnapshot {
            judgment_usage: Some(JudgmentUsageSnapshot::from_ledger(judgment_facts(3))),
            ..Default::default()
        };
        for policy in ["live_only", "local_only"] {
            let request =
                IntrospectRequest::from_args(&serde_json::json!({"source_policy":policy}));
            let report = build_introspect_report(&snapshot, &request);
            let usage = report.judgment_usage.unwrap();
            assert_eq!(usage.coverage, JudgmentUsageCoverage::SourceExcluded);
            assert!(usage.attempts.is_empty());
            let text = render_introspect_request(&snapshot, &request);
            assert!(!text.contains("actual-provider"));
        }
        let request =
            IntrospectRequest::from_args(&serde_json::json!({"horizon":"turn", "depth":"hint"}));
        let text = render_introspect_request(&snapshot, &request);
        assert!(text.contains("session ledger at read time"));
        assert!(text.contains("omitted_attempts=1"));
        assert!(text.contains("actual-provider"));
    }

    #[test]
    fn judgment_usage_capture_overflow_retains_exact_facts_as_lower_bounds() {
        use astra_turn_types::{ExplainAnalyzeAuxiliaryUsageStatusV1, ExplainAnalyzeUsageBasisV1};
        let mut facts = judgment_facts(1);
        facts.truncated = true;
        facts.attempts[0].usage_status = ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderExact;
        let usage = facts.attempts[0].usage.as_mut().unwrap();
        usage.basis = ExplainAnalyzeUsageBasisV1::ProviderExact;
        usage.cache_creation_tokens = Some(0);
        let snapshot = JudgmentUsageSnapshot::from_ledger(facts);
        assert_eq!(snapshot.coverage, JudgmentUsageCoverage::CaptureTruncated);
        assert_eq!(snapshot.observed_attempts, Some(1));
        assert_eq!(snapshot.attempts_without_complete_usage, Some(0));
        assert_eq!(snapshot.known_input_tokens, Some(30));
        assert_eq!(snapshot.known_output_tokens, Some(5));
        assert!(!snapshot.input_complete && !snapshot.output_complete);
        assert!(!snapshot.groups[0].input_complete && !snapshot.groups[0].output_complete);
        assert_eq!(
            snapshot.omitted_attempts, 0,
            "capture omissions have unknown count"
        );
        assert!(snapshot.render().contains("total_output=at_least_5"));
        let report = build_introspect_report(
            &IntrospectSnapshot {
                judgment_usage: Some(snapshot),
                ..Default::default()
            },
            &IntrospectRequest::from_args(&serde_json::json!({"depth":"hint", "format":"json"})),
        );
        assert_eq!(
            report.data_coverage.providers["judgment_inference_ledger"].status,
            "partial"
        );

        let mut empty = judgment_facts(0);
        empty.truncated = true;
        let empty = JudgmentUsageSnapshot::from_ledger(empty);
        assert_eq!(empty.coverage, JudgmentUsageCoverage::CaptureTruncated);
        assert!(!empty.output_complete);
        assert!(empty.render().contains("total_output=at_least_0"));
    }

    #[test]
    fn judgment_usage_distinguishes_unavailable_empty_and_truncated_identities() {
        let unavailable =
            JudgmentUsageSnapshot::from_ledger(astra_turn_types::ExplainAnalyzeAuxiliaryUsageV1 {
                available: false,
                truncated: false,
                attempts: vec![],
            });
        assert_eq!(unavailable.observed_attempts, None);
        assert_eq!(
            JudgmentUsageSnapshot::from_ledger(judgment_facts(0)).observed_attempts,
            Some(0)
        );
        let mut facts = judgment_facts(1);
        facts.attempts[0].provider = "界".repeat(1000);
        let usage = JudgmentUsageSnapshot::from_ledger(facts);
        assert_eq!(usage.truncated_identity_fields, 2);
        assert_eq!(usage.attempts[0].provider.chars().count(), 128);
        assert_eq!(
            usage
                .bounded(ObservationDepth::Hint)
                .truncated_identity_fields,
            2
        );
        assert!(serde_json::to_string(&usage).unwrap().len() < 3000);
    }
    use crate::context_feedback::{
        RuntimePolicyFeedbackEntry, RuntimePolicyFeedbackSet, RuntimePolicyRecommendation,
        RuntimePolicySignal, RuntimePolicyStage, RuntimePolicySubject,
    };
    use astra_core::EvidenceRef;

    #[test]
    fn turn_budget_distinguishes_slice_from_absolute_ceiling() {
        let mut snapshot = IntrospectSnapshot::default();
        assert_eq!(turn_budget_label(&snapshot), "not_yet_observed");
        snapshot.runtime_feedback = Some(test_runtime_feedback(1, 6, 18));
        assert_eq!(
            turn_budget_label(&snapshot),
            "session_turn=1 llm_rounds_completed=6 slice_boundary=24 slice_remaining=18 absolute_round_ceiling=none"
        );
        snapshot
            .runtime_feedback
            .as_mut()
            .unwrap()
            .progress
            .absolute_round_ceiling = Some(50);
        assert_eq!(
            turn_budget_label(&snapshot),
            "session_turn=1 llm_rounds_completed=6 slice_boundary=24 slice_remaining=18 absolute_round_ceiling=50"
        );
    }

    fn sample_snapshot() -> IntrospectSnapshot {
        IntrospectSnapshot {
            runtime_feedback: Some(test_runtime_feedback(8, 8, 12)),
            snapshot_age_turns: 0,
            alerts: vec![
                "cache_regression: hit rate dropped 20% in 3 turns".into(),
                "tool_health: bash error rate >30%".into(),
            ],
            tool_health: vec![
                ToolHealthEntry {
                    name: "bash".into(),
                    calls: 15,
                    errors: 5,
                    input_validation_failures: 2,
                    avg_ms: 2300,
                    avoidance_advised: false,
                    consecutive_failures: 0,
                    last_failure_category: None,
                },
                ToolHealthEntry {
                    name: "read_file".into(),
                    calls: 22,
                    errors: 0,
                    input_validation_failures: 0,
                    avg_ms: 12,
                    avoidance_advised: false,
                    consecutive_failures: 0,
                    last_failure_category: None,
                },
                ToolHealthEntry {
                    name: "grep".into(),
                    calls: 8,
                    errors: 1,
                    input_validation_failures: 0,
                    avg_ms: 45,
                    avoidance_advised: true,
                    consecutive_failures: 3,
                    last_failure_category: Some("Timeout".into()),
                },
            ],
            working_memory_summary: "Goal: implement streaming resume".into(),
            lifecycle_summary:
                "### Turn-start session execution state\nresume pending: [plan-resume] goal=\"Fix auth\""
                    .into(),
            capacity_provider_coverage: Vec::new(),
            tool_admission: Vec::new(),
            semantic_cache_decisions: Vec::new(),
            invocation_lifecycle: None,
            judgment_usage: None,
            semantic_judgments: None,
            tool_result_judgments: None,
            recent_rounds: Vec::new(),
            step_latency: Vec::new(),
            volatile_pending: Vec::new(),
            stall_state: StallSnapshotSummary::default(),
            injection_freshness: Vec::new(),
            current_round: 0,
            tool_errors: Vec::new(),
            circuit_breaker: None,
        }
    }

    #[test]
    fn hint_is_single_line() {
        let output = render_introspect(&sample_snapshot(), IntrospectTextDepth::Hint);
        assert!(
            !output.contains('\n'),
            "hint must be a single line: {output}"
        );
        assert!(output.contains("pressure=72%"));
        assert!(output.contains("prompt_cache_read_share=66%"));
        assert!(output.contains("prompt_cache_scope=current_runtime_snapshot"));
        assert!(output.contains("input_total=145000"));
        assert!(output.contains("cached_read=95000"));
        assert!(output.contains("prompt_cache_stable_prefix_tokens=95000"));
        assert!(output.contains("prompt_cache_read_over_stable_prefix=1.00"));
        assert!(output.contains("cache_create=8000"));
        assert!(!output.contains("cache=66%"));
        assert!(output.contains("turns=session_turn=8 llm_rounds_completed=8 slice_boundary=20 slice_remaining=12 absolute_round_ceiling=none"));
        assert!(output.contains("alerts=2"));
        assert!(output.contains("model=deepseek-v4-flash"));
        assert!(output.contains("topology=server_only"));
    }

    #[test]
    fn policy_feedback_subject_is_visible_at_every_text_depth() {
        let mut snapshot = sample_snapshot();
        snapshot
            .runtime_feedback
            .as_mut()
            .expect("runtime feedback")
            .policy_feedback = RuntimePolicyFeedbackSet::Evaluated {
            recovery: None,
            schema_version: RuntimePolicyFeedbackSet::SCHEMA_VERSION,
            revision: 4,
            evaluated_at_round: 8,
            subject: RuntimePolicySubject::WorkItem {
                attempt_id: "attempt-1".to_string(),
                item_id: "item-2".to_string(),
                item_revision: 3,
                objective: "Inspect one target".to_string(),
                expected_result: "One verified fact".to_string(),
            },
            entries: vec![RuntimePolicyFeedbackEntry {
                signal: RuntimePolicySignal::ReadCoverageOverlap,
                stage: RuntimePolicyStage::Converge,
                observed_at_round: 8,
                evidence_count: 9,
                recommendation: RuntimePolicyRecommendation::ReviewReadCoverage,
            }],
        };

        for depth in [
            IntrospectTextDepth::Hint,
            IntrospectTextDepth::Summary,
            IntrospectTextDepth::Full,
        ] {
            let output = render_introspect(&snapshot, depth);
            assert!(output.contains("server_only"), "{depth:?}: {output}");
            assert!(output.contains("work_item=item-2@3"), "{depth:?}: {output}");
            assert!(
                output.contains("ReadCoverageOverlap/Converge"),
                "{depth:?}: {output}"
            );
        }
    }

    #[test]
    fn semantic_cache_decisions_are_visible_without_breaking_hint_geometry() {
        let mut snapshot = sample_snapshot();
        snapshot.semantic_cache_decisions = vec![SemanticCacheDecisionSnapshotEntry {
            tool_name: "catalog_read".to_string(),
            state: "freshness_unavailable".to_string(),
            key_id: None,
        }];

        let hint = render_introspect(&snapshot, IntrospectTextDepth::Hint);
        assert!(!hint.contains('\n'));
        assert!(hint.contains("semantic_cache_decisions=1"));
        let summary = render_introspect(&snapshot, IntrospectTextDepth::Summary);
        assert!(summary.contains("catalog_read=freshness_unavailable"));
        let full = render_introspect(&snapshot, IntrospectTextDepth::Full);
        assert!(full.contains("## Semantic Read Cache Decisions"));
    }

    #[test]
    fn durable_invocation_lifecycle_is_visible_in_text_and_json_evidence() {
        let mut snapshot = sample_snapshot();
        snapshot.invocation_lifecycle = Some(InvocationLifecycleSnapshot {
            run_id: Some("run-1".to_string()),
            hot_total: 3,
            prepared: 1,
            dispatched: 0,
            succeeded: 1,
            failed: 0,
            rejected: 0,
            outcome_unknown: 1,
            rejected_without_dispatch: 0,
            archive_chunks: 2,
            durable_artifact_references: 4,
            reconciliation_events: 1,
            compaction_deferred_events: 1,
            compaction_cursor_generation: Some(7),
            compaction_cursor_updated_at: Some("2026-07-16 10:00:00.000000".to_string()),
        });

        let hint = render_introspect(&snapshot, IntrospectTextDepth::Hint);
        assert!(hint.contains("invocation_lifecycle=hot:3"));
        let full = render_introspect(&snapshot, IntrospectTextDepth::Full);
        assert!(full.contains("## Durable Invocation Lifecycle"));
        assert!(full.contains("outcome_unknown=1"));
        let request = IntrospectRequest::from_args(&serde_json::json!({
            "facet": "session",
            "depth": "diagnostic",
            "format": "json"
        }));
        let report: serde_json::Value =
            serde_json::from_str(&render_introspect_request(&snapshot, &request)).unwrap();
        assert!(
            report["observations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| {
                    entry["kind"] == "durable_invocation_lifecycle"
                        && entry["severity"] == "warning"
                })
        );
        assert!(
            report["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| { entry["source"] == "matrixone.tool_invocation_lifecycle" })
        );
    }

    #[test]
    fn summary_includes_key_metrics_and_top_alerts() {
        let output = render_introspect(&sample_snapshot(), IntrospectTextDepth::Summary);
        assert!(output.contains("## Current Runtime Snapshot"));
        assert!(output.contains("Scope: current live runtime snapshot"));
        assert!(output.contains("Prompt cache read share: 66%"));
        assert!(output.contains(
            "Prompt tokens: input_total=145000 fresh=42000 cached_read=95000 cache_create=8000"
        ));
        assert!(!output.contains("Cache: 66%"));
        assert!(output.contains("cache_regression"));
        assert!(output.contains("Current model: deepseek-v4-flash"));
        assert!(output.contains("Provider context window: 1000000 tokens"));
        assert!(output.contains("Effective input budget: 132000/800000 tokens"));
        assert!(output.contains("Goal: implement streaming resume"));
        assert!(output.contains("### Turn-start session execution state"));
        // Should NOT contain full tool table
        assert!(!output.contains("| Tool |"));
    }

    #[test]
    fn full_includes_tool_health_table() {
        let output = render_introspect(&sample_snapshot(), IntrospectTextDepth::Full);
        assert!(output.contains("## Tool Health"));
        assert!(output.contains("| bash |"));
        assert!(output.contains("| read_file |"));
        assert!(output.contains("Input rejects"));
        assert!(output.contains("| bash | 15 | 5 | 2 |"));
    }

    #[test]
    fn summary_and_json_include_tool_admission_metadata() {
        let snap = IntrospectSnapshot {
            tool_admission: vec![ToolAdmissionSnapshotEntry {
                tool_name: "web_fetch".to_string(),
                provider_visible: Some(true),
                visible: true,
                selected_offer_id: Some("web_fetch@edge-1".to_string()),
                selected_route: "EdgeBound".to_string(),
                hidden_reason: None,
                candidates: vec![
                    ToolAdmissionCandidateSnapshotEntry {
                        offer_id: "web_fetch@edge-1".to_string(),
                        provider_type: "edge_capacity".to_string(),
                        provider_id: "edge-1".to_string(),
                        executor_id: "edge-1".to_string(),
                        placement: "edge:edge-1".to_string(),
                        scope: "workspace".to_string(),
                        authority: "read_write".to_string(),
                        schema_digest: "sha256:edge".to_string(),
                        route: "EdgeBound".to_string(),
                        readiness: "ready".to_string(),
                        selected: true,
                        reason: "Selected".to_string(),
                    },
                    ToolAdmissionCandidateSnapshotEntry {
                        offer_id: "web_fetch@server-builtin".to_string(),
                        provider_type: "server_service".to_string(),
                        provider_id: "server-builtin".to_string(),
                        executor_id: "server-service".to_string(),
                        placement: "server".to_string(),
                        scope: "session".to_string(),
                        authority: "none".to_string(),
                        schema_digest: "sha256:server".to_string(),
                        route: "ServerRuntime".to_string(),
                        readiness: "ready".to_string(),
                        selected: false,
                        reason: "CurrentProviderPreferred".to_string(),
                    },
                ],
            }],
            ..Default::default()
        };

        let summary = render_introspect(&snap, IntrospectTextDepth::Summary);
        assert!(summary.contains("Route readiness: visible=1/1 hidden=0"));
        assert!(summary.contains("Provider wire tool surface: visible=1 tools=[web_fetch]"));

        let req = IntrospectRequest {
            format: IntrospectFormat::Json,
            ..Default::default()
        };
        let json = render_introspect_request(&snap, &req);
        assert!(json.contains("\"kind\":\"tool_admission\""), "{json}");
        assert!(json.contains("web_fetch@edge-1"), "{json}");
    }

    #[test]
    fn render_json_envelope_contains_shared_observation_shape() {
        let mut snap = sample_snapshot();
        snap.tool_errors.push(ToolErrorEntry {
            tool: "bash".into(),
            signature_hint: "command timed out".into(),
            failure_category: Some("tool_timeout".into()),
            error_preview: Some("command timed out after 30s".into()),
            at_epoch: 1,
            error_message: "command timed out after 30s".into(),
            file_path: None,
            file_range: None,
            turn: 5,
            round: 2,
        });

        let req = IntrospectRequest::from_args(&serde_json::json!({
            "facet": "execution/errors",
            "format": "json"
        }));
        let out = render_introspect_request(&snap, &req);
        let report: IntrospectReport = serde_json::from_str(&out).expect("json report");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("json value");

        assert_eq!(report.schema_version, INTROSPECT_REPORT_SCHEMA_VERSION);
        assert_eq!(report.tool, "introspect");
        assert_eq!(parsed["schema_version"], INTROSPECT_REPORT_SCHEMA_VERSION);
        assert_eq!(parsed["tool"], "introspect");
        assert_eq!(
            parsed["runtime_feedback"]["identity"]["topology"],
            "server_only"
        );
        assert_eq!(
            report
                .runtime_feedback
                .as_ref()
                .map(|frame| frame.identity.topology),
            Some(astra_services::ModelRequestTopology::ServerOnly)
        );
        assert_eq!(parsed["topic"], parsed["view"]["topic"]);
        assert_eq!(parsed["facet"], parsed["view"]["facet"]);
        assert_eq!(parsed["depth"], parsed["view"]["depth"]);
        assert_eq!(parsed["horizon"], parsed["view"]["horizon"]);
        assert_eq!(parsed["data_coverage"], parsed["view"]["data_coverage"]);
        assert_eq!(report.view.topic, "execution");
        assert_eq!(report.view.facet, "errors");
        assert!(
            report
                .summary
                .contains("snapshot_cutoff=before_current_introspect_execution")
        );
        assert!(report.summary.contains("recent tool errors"));
        assert!(
            report
                .observations
                .iter()
                .any(|observation| observation.ref_id
                    == "urn:astra:observation:local:introspect:execution:error:0"
                    && observation.kind == "tool_error:tool_timeout")
        );
        assert!(report.evidence.iter().any(
            |evidence| evidence.ref_id == "urn:astra:context:local:introspect:runtime_snapshot"
        ));
        assert_eq!(report.view.data_coverage.overall, "fresh");
        assert!(
            report
                .view
                .data_coverage
                .providers
                .contains_key("live_runtime")
        );
        assert!(!report.budget_result.truncated);
        assert_report_refs_are_valid(&report);
    }

    #[test]
    fn historical_horizon_returns_a_labeled_recent_live_projection() {
        let request = IntrospectRequest::from_args(&serde_json::json!({
            "facet": "trace",
            "horizon": "session"
        }));
        let text = render_introspect_request(&sample_snapshot(), &request);
        assert!(text.contains("Introspect Live Projection"), "{text}");
        assert!(text.contains("requested_horizon=session"), "{text}");
        assert!(text.contains("coverage=recent-only"), "{text}");
        assert!(text.contains("use reflect"), "{text}");
        assert!(
            text.contains("snapshot_cutoff=before_current_introspect_execution"),
            "{text}"
        );
        assert!(
            text.contains("No rounds recorded yet in this turn"),
            "{text}"
        );

        let json_request = IntrospectRequest::from_args(&serde_json::json!({
            "facet": "trace",
            "horizon": "session",
            "format": "json"
        }));
        let report: IntrospectReport = serde_json::from_str(&render_introspect_request(
            &sample_snapshot(),
            &json_request,
        ))
        .expect("live projection report");
        assert_eq!(report.data_coverage.overall, "partial");
        assert_eq!(report.horizon, "recent");
        assert_eq!(report.view.horizon, "recent");
        assert!(report.summary.contains("use reflect"));
        assert!(
            report
                .data_coverage
                .warnings
                .iter()
                .any(|warning| { warning.contains("requested historical horizon=session") })
        );
        assert!(!report.evidence.is_empty());
    }

    #[test]
    fn render_json_edge_only_facet_reports_unavailable_without_unrelated_runtime_noise() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "facet": "session_memory",
            "format": "json"
        }));
        let out = render_introspect_request(&sample_snapshot(), &req);
        let report: IntrospectReport = serde_json::from_str(&out).expect("json report");

        assert_eq!(report.view.facet, "session_memory");
        assert_eq!(
            report.view.data_coverage.source,
            "edge_local_artifacts_unavailable"
        );
        assert_eq!(report.view.data_coverage.events, 0);
        assert!(
            report
                .view
                .data_coverage
                .warnings
                .iter()
                .any(|warning| warning.contains("Edge-local"))
        );
        assert_eq!(report.observations.len(), 1);
        assert_eq!(
            report.observations[0].kind, "data_surface_unavailable",
            "edge-only JSON must not mix unrelated runtime/tool observations: {report:?}"
        );
        assert!(report.evidence.is_empty());
        assert!(report.action_hints.is_empty());
        assert_eq!(
            report.view.data_coverage.providers["local_journal"].status,
            "missing"
        );
        assert_report_refs_are_valid(&report);
    }

    #[test]
    fn render_json_action_hints_reference_matching_tool_observation_only() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "format": "json"
        }));
        let out = render_introspect_request(&sample_snapshot(), &req);
        let report: IntrospectReport = serde_json::from_str(&out).expect("json report");

        let hint = report
            .action_hints
            .iter()
            .find(|hint| hint.summary.contains("grep"))
            .expect("grep avoidance hint");
        assert_eq!(
            hint.observation_refs,
            vec!["urn:astra:observation:local:introspect:execution:tool:grep"]
        );
        assert!(
            !hint
                .observation_refs
                .iter()
                .any(|reference| reference.contains(":runtime:alert:")),
            "tool action hints must not cite unrelated runtime alerts: {hint:?}"
        );
        assert_report_refs_are_valid(&report);
    }

    #[test]
    fn render_json_hint_budget_truncates_without_dangling_action_refs() {
        let mut snap = sample_snapshot();
        snap.alerts.clear();
        snap.tool_health = (0..8)
            .map(|idx| ToolHealthEntry {
                name: format!("tool_{idx}"),
                calls: 10 + idx,
                errors: 3,
                input_validation_failures: 0,
                avg_ms: 100,
                avoidance_advised: true,
                consecutive_failures: 3,
                last_failure_category: Some("timeout".into()),
            })
            .collect();

        let req = IntrospectRequest::from_args(&serde_json::json!({
            "depth": "hint",
            "format": "json"
        }));
        let out = render_introspect_request(&snap, &req);
        let report: IntrospectReport = serde_json::from_str(&out).expect("json report");

        assert_eq!(report.view.depth, "hint");
        assert_eq!(report.observations.len(), 3);
        assert_eq!(report.action_hints.len(), 2);
        assert!(report.budget_result.truncated);
        assert_eq!(report.budget_result.omitted.observations, 6);
        assert_eq!(report.budget_result.omitted.action_hints, 3);
        assert_report_refs_are_valid(&report);
    }

    #[test]
    fn render_json_cloud_only_with_context_reports_missing_providers() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "source_policy": "cloud_only",
            "include_context": true,
            "format": "json"
        }));
        let out = render_introspect_request(&sample_snapshot(), &req);
        let report: IntrospectReport = serde_json::from_str(&out).expect("json report");

        assert_eq!(report.view.data_coverage.overall, "partial");
        assert_eq!(report.view.data_coverage.source, "cloud_runtime_snapshot");
        assert_eq!(
            report.view.data_coverage.providers["cloud_events"].status,
            "missing"
        );
        assert_eq!(
            report.view.data_coverage.providers["visible_context"].status,
            "missing"
        );
        assert!(
            report
                .view
                .data_coverage
                .warnings
                .iter()
                .any(|warning| warning.contains("include_context requested"))
        );
        assert_report_refs_are_valid(&report);
    }

    fn assert_report_refs_are_valid(report: &IntrospectReport) {
        let observation_ids = report
            .observations
            .iter()
            .map(|observation| observation.ref_id.as_str())
            .collect::<BTreeSet<_>>();
        for observation in &report.observations {
            EvidenceRef::parse(&observation.ref_id).unwrap_or_else(|err| {
                panic!("invalid observation ref {}: {err}", observation.ref_id)
            });
            for evidence_ref in &observation.evidence_refs {
                EvidenceRef::parse(evidence_ref).unwrap_or_else(|err| {
                    panic!("invalid observation evidence ref {evidence_ref}: {err}")
                });
            }
        }
        for evidence in &report.evidence {
            EvidenceRef::parse(&evidence.ref_id)
                .unwrap_or_else(|err| panic!("invalid evidence ref {}: {err}", evidence.ref_id));
        }
        for hint in &report.action_hints {
            for observation_ref in &hint.observation_refs {
                EvidenceRef::parse(observation_ref).unwrap_or_else(|err| {
                    panic!("invalid action hint ref {observation_ref}: {err}")
                });
                assert!(
                    observation_ids.contains(observation_ref.as_str()),
                    "action hint ref must point to a retained observation: {observation_ref}"
                );
            }
        }
    }

    #[test]
    fn unavailable_message_explains_missing_edge_provider() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "facet": "session_memory"
        }));
        let out = render_introspect_request(&IntrospectSnapshot::default(), &req);
        assert!(out.contains("Introspect Unavailable"), "{out}");
        assert!(out.contains("facet=session_memory"), "{out}");
        assert!(out.contains("CLI/Edge-local session artifacts"), "{out}");
        assert!(
            out.contains("no CLI/Edge-local artifact provider is attached"),
            "{out}"
        );
    }

    #[test]
    fn unavailable_message_explains_source_policy_exclusion() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "facet": "session_memory",
            "source_policy": "cloud_only"
        }));
        let out = render_introspect_request(&IntrospectSnapshot::default(), &req);
        assert!(out.contains("Introspect Unavailable"), "{out}");
        assert!(out.contains("source_policy=cloud_only"), "{out}");
        assert!(
            out.contains("requested source_policy does not allow CLI/Edge-local artifacts"),
            "{out}"
        );
    }

    #[test]
    fn empty_snapshot_renders_stable_empty_state_contract() {
        let empty = IntrospectSnapshot::default();
        let hint = render_introspect(&empty, IntrospectTextDepth::Hint);
        assert_eq!(
            hint,
            "runtime_feedback=not_yet_observed alerts=0 providers= tool_admission=visible=0/0 hidden=0"
        );
        let full = render_introspect(&empty, IntrospectTextDepth::Full);
        assert!(full.contains("## Current Runtime Snapshot"));
        assert!(full.contains("Runtime feedback: not yet observed"));
        assert!(!full.contains("Cache: 0%"));
        assert!(!full.contains("## Tool Health"));
        assert!(!full.contains("Current model:"));
    }

    #[test]
    fn zero_remaining_is_exhausted_not_unlimited() {
        let snap = IntrospectSnapshot {
            runtime_feedback: Some(test_runtime_feedback(3, 3, 0)),
            ..Default::default()
        };

        let hint = render_introspect(&snap, IntrospectTextDepth::Hint);
        assert!(hint.contains("remaining=0"), "got: {hint}");
        assert!(!hint.contains('∞'), "got: {hint}");

        let summary = render_introspect(&snap, IntrospectTextDepth::Summary);
        assert!(summary.contains("remaining=0"), "got: {summary}");
    }

    #[test]
    fn stale_snapshot_renders_age_marker() {
        let snap = IntrospectSnapshot {
            snapshot_age_turns: 2,
            ..Default::default()
        };

        let hint = render_introspect(&snap, IntrospectTextDepth::Hint);
        assert!(
            hint.contains("snapshot_age_turns=2"),
            "hint should surface staleness: {hint}"
        );

        let summary = render_introspect(&snap, IntrospectTextDepth::Summary);
        assert!(
            summary.contains("Snapshot age: 2 turn(s)"),
            "summary should surface staleness: {summary}"
        );
    }

    #[test]
    fn runtime_snapshot_renders_capacity_provider_coverage() {
        let snap = IntrospectSnapshot {
            capacity_provider_coverage: vec![
                CapacityProviderCoverageEntry::ready(
                    astra_runtime_env::CapacityProviderType::ServerService,
                    "server-builtin",
                    vec!["web_fetch".into()],
                ),
                CapacityProviderCoverageEntry::unavailable(
                    astra_runtime_env::CapacityProviderType::Sandbox,
                    "workspace-executor",
                    astra_runtime_env::CapacityProviderStatus::Unbound,
                    "no_workspace_provider_bound",
                ),
            ],
            ..Default::default()
        };

        let hint = render_introspect(&snap, IntrospectTextDepth::Hint);
        assert!(hint.contains("providers=server_service:ready"));
        assert!(hint.contains("sandbox:unbound (workspace executor not bound)"));
        assert!(!hint.contains("no_workspace_provider_bound"));

        let summary = render_introspect(&snap, IntrospectTextDepth::Summary);
        assert!(summary.contains("Capacity providers: server_service:ready"));
        assert!(summary.contains("sandbox:unbound (workspace executor not bound)"));
        assert!(!summary.contains("no_workspace_provider_bound"));
    }

    #[test]
    fn many_alerts_truncated_in_summary_shown_in_full() {
        let mut s = sample_snapshot();
        s.alerts = (0..10).map(|i| format!("alert-{i}")).collect();
        let summary = render_introspect(&s, IntrospectTextDepth::Summary);
        assert!(summary.contains("(+7 more)"));
        let full = render_introspect(&s, IntrospectTextDepth::Full);
        assert!(full.contains("## All Alerts"));
        assert!(full.contains("alert-9"));
    }

    // ── Task #46: recent-rounds / volatile / stall renderers ──

    #[test]
    fn render_recent_rounds_empty_state_message() {
        let snap = IntrospectSnapshot::default();
        let out = render_recent_rounds(&snap);
        assert!(out.contains("No rounds recorded"), "got: {out}");
    }

    #[test]
    fn render_recent_rounds_tabulates_and_summarizes() {
        let snap = IntrospectSnapshot {
            recent_rounds: vec![
                RoundSnapshotEntry {
                    turn: 3,
                    round: 0,
                    provider: "anthropic".into(),
                    model: "claude".into(),
                    prompt_tokens: 100,
                    cache_read_tokens: 7000,
                    tool_calls_returned: 2,
                    tool_call_names: vec!["bash".into(), "read_file".into()],
                    duration_ms: 1500,
                    finish_reason: Some("tool_calls".into()),
                    ..Default::default()
                },
                RoundSnapshotEntry {
                    turn: 3,
                    round: 1,
                    provider: "anthropic".into(),
                    model: "claude".into(),
                    prompt_tokens: 200,
                    cache_read_tokens: 7300,
                    tool_calls_returned: 0,
                    tool_call_names: vec![],
                    duration_ms: 900,
                    finish_reason: Some("stop".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let out = render_recent_rounds(&snap);
        assert!(out.contains("t3_r0"));
        assert!(out.contains("t3_r1"));
        assert!(out.contains("bash,read_file"));
        // Summary line has scoped prompt-cache read share.
        assert!(out.contains("Ring: 2 rounds"));
        assert!(out.contains("prompt_cache_read_share"));
        assert!(out.contains("cached_read=14300/input_total=14600"));
        assert!(!out.contains("cache_hit="));
    }

    #[test]
    fn render_step_latency_attributes_model_wait_vs_tool_time() {
        let snap = IntrospectSnapshot {
            step_latency: vec![StepLatencySnapshotEntry {
                step_id: "turn-1-step-3".into(),
                total_ms: Some(8_978),
                pre_tool_wait_ms: Some(8_000),
                first_tool_name: Some("bash".into()),
                tool_call_count: 1,
                skipped_tool_count: 0,
                tool_execution_ms: 8,
                max_tool_execution_ms: 8,
                terminal_event_kind: Some("StepIncomplete".into()),
                dominant_phase: "model_wait".into(),
            }],
            ..Default::default()
        };

        let out = render_step_latency(&snap);

        assert!(out.contains("## Step Latency"));
        assert!(out.contains("turn-1-step-3"));
        assert!(out.contains(
            "| turn-1-step-3 | 8978 | 8000 | 8 | 8 | 1 | 0 | model_wait | bash | StepIncomplete |"
        ));
        assert!(out.contains("Latest: dominant=model_wait pre_tool=8000 tool_ms=8"));
    }

    #[test]
    fn render_step_latency_handles_minimal_entry() {
        let snap = IntrospectSnapshot {
            step_latency: vec![StepLatencySnapshotEntry {
                step_id: "s1".into(),
                total_ms: Some(250),
                dominant_phase: "no_tool".into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let out = render_step_latency(&snap);

        assert!(out.contains("## Step Latency"));
        assert!(out.contains("| s1 | 250 |"));
    }

    #[test]
    fn render_volatile_pending_empty_and_populated() {
        let empty = IntrospectSnapshot::default();
        assert!(render_volatile_pending(&empty).contains("Empty"));

        let snap = IntrospectSnapshot {
            volatile_pending: vec![VolatileSnapshotEntry {
                kind: "PolicyAdvisory".into(),
                content: "Repeated operation observed; result freshness is unknown.".into(),
                round_index: 2,
            }],
            ..Default::default()
        };
        let out = render_volatile_pending(&snap);
        assert!(out.contains("PolicyAdvisory"));
        assert!(out.contains("round 2"));
    }

    #[test]
    fn render_stall_state_empty_and_observed() {
        let healthy = IntrospectSnapshot::default();
        assert!(render_stall_state(&healthy).contains("No events"));

        let mut snap = IntrospectSnapshot::default();
        snap.stall_state.introspection_count = 1;
        snap.stall_state.advisory_signals = vec!["parallel_batching_force".into()];
        snap.stall_state.events = vec!["sig_stall @ turn 5".into()];
        let out = render_stall_state(&snap);
        assert!(out.contains("Circuit-breaker introspections: 1"));
        assert!(out.contains("sig_stall @ turn 5"));
        assert!(out.contains("parallel_batching_force"));
    }

    #[test]
    fn stall_introspection_count_is_visible_without_events_or_advisories() {
        let snapshot = IntrospectSnapshot {
            stall_state: StallSnapshotSummary {
                introspection_count: 2,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(render_stall_state(&snapshot).contains("Circuit-breaker introspections: 2"));
        let wire = serde_json::to_value(&snapshot.stall_state).unwrap();
        assert!(wire.get("nudge_count").is_none());
    }

    #[test]
    fn render_all_includes_every_section() {
        let mut snap = sample_snapshot();
        snap.recent_rounds.push(RoundSnapshotEntry {
            turn: 1,
            round: 0,
            prompt_tokens: 10,
            cache_read_tokens: 50,
            ..Default::default()
        });
        snap.volatile_pending.push(VolatileSnapshotEntry {
            kind: "PolicyAdvisory".into(),
            content: "Repeated operation observed.".into(),
            round_index: 0,
        });
        snap.injection_freshness = vec![ChannelFreshness {
            channel: InjectionChannel::Lessons,
            status: ChannelStatus::Fresh { rounds_alive: 0 },
            preview: "lesson preview".into(),
            first_seen_round: Some(0),
        }];
        let out = render_all(&snap);
        assert!(out.contains("## Current Runtime Snapshot"));
        assert!(out.contains("## Recent Rounds"));
        assert!(out.contains("## Volatile Lane"));
        assert!(out.contains("## Stall / Loop-Guard"));
        assert!(out.contains("## Injection Freshness"));
    }

    // ── Injection freshness (facet=noise) renderer ──

    #[test]
    fn render_injection_freshness_empty_emits_empty_state_message() {
        let snap = IntrospectSnapshot::default();
        let out = render_injection_freshness(&snap);
        assert!(
            out.contains("No injections tracked yet"),
            "empty state message missing: {out}"
        );
    }

    #[test]
    fn render_injection_freshness_marks_stale_channels() {
        let snap = IntrospectSnapshot {
            current_round: 58,
            injection_freshness: vec![
                ChannelFreshness {
                    channel: InjectionChannel::RecentFailingTests,
                    status: ChannelStatus::Stale { rounds_alive: 58 },
                    preview: "could not find Cargo.toml in /home/.../astra".into(),
                    first_seen_round: Some(0),
                },
                ChannelFreshness {
                    channel: InjectionChannel::OutcomeBias,
                    status: ChannelStatus::Fresh { rounds_alive: 2 },
                    preview: "bash ↑0.10 · git ↑0.10 · run_script ↓0.08".into(),
                    first_seen_round: Some(56),
                },
                ChannelFreshness {
                    channel: InjectionChannel::Lessons,
                    status: ChannelStatus::Untracked,
                    preview: String::new(),
                    first_seen_round: None,
                },
                ChannelFreshness {
                    channel: InjectionChannel::VolatilePending,
                    status: ChannelStatus::Empty {
                        first_seen_round: 57,
                    },
                    preview: String::new(),
                    first_seen_round: Some(57),
                },
            ],
            ..Default::default()
        };
        let out = render_injection_freshness(&snap);
        assert!(out.contains("## Injection Freshness (round 58)"), "{out}");
        assert!(
            out.contains("recent_failing_tests") && out.contains("⚠ STALE"),
            "stale channel must be flagged: {out}"
        );
        assert!(
            out.contains("outcome_bias") && out.contains("fresh"),
            "{out}"
        );
        assert!(
            out.contains("lessons") && out.contains("untracked"),
            "{out}"
        );
        assert!(
            out.contains("volatile_pending") && out.contains("empty"),
            "{out}"
        );
        assert!(
            out.contains("Cargo.toml"),
            "preview content must render: {out}"
        );
        assert!(
            out.contains("1/2 channels unchanged"),
            "summary should count 1 stale of 2 tracked (non-untracked, non-empty): {out}"
        );
    }

    #[test]
    fn render_injection_freshness_all_fresh_summary() {
        let snap = IntrospectSnapshot {
            current_round: 3,
            injection_freshness: vec![ChannelFreshness {
                channel: InjectionChannel::Lessons,
                status: ChannelStatus::Fresh { rounds_alive: 1 },
                preview: "recent lesson".into(),
                first_seen_round: Some(2),
            }],
            ..Default::default()
        };
        let out = render_injection_freshness(&snap);
        assert!(
            out.contains("1/1 tracked channels are fresh"),
            "all-fresh summary line missing: {out}"
        );
        assert!(
            !out.contains("⚠ STALE"),
            "no stale marker should appear: {out}"
        );
    }

    #[test]
    fn render_injection_freshness_escapes_pipe_in_preview() {
        let snap = IntrospectSnapshot {
            current_round: 2,
            injection_freshness: vec![ChannelFreshness {
                channel: InjectionChannel::VolatilePending,
                status: ChannelStatus::Fresh { rounds_alive: 0 },
                preview: "pipe | char in content".into(),
                first_seen_round: Some(2),
            }],
            ..Default::default()
        };
        let out = render_injection_freshness(&snap);
        assert!(
            out.contains("pipe \\| char"),
            "pipe character in preview must be escaped so the markdown table renders correctly: {out}"
        );
    }

    #[test]
    fn render_injection_freshness_only_empty_channels_reports_no_injection() {
        let snap = IntrospectSnapshot {
            current_round: 5,
            injection_freshness: vec![
                ChannelFreshness {
                    channel: InjectionChannel::Lessons,
                    status: ChannelStatus::Empty {
                        first_seen_round: 0,
                    },
                    preview: String::new(),
                    first_seen_round: Some(0),
                },
                ChannelFreshness {
                    channel: InjectionChannel::OutcomeBias,
                    status: ChannelStatus::Untracked,
                    preview: String::new(),
                    first_seen_round: None,
                },
            ],
            ..Default::default()
        };
        let out = render_injection_freshness(&snap);
        assert!(
            out.contains("runtime has not injected anything"),
            "no-injection summary missing: {out}"
        );
    }

    // ── Tests for render_errors ──────────────────────────────────────────

    #[test]
    fn render_errors_empty_reports_no_failures() {
        let snap = IntrospectSnapshot::default();
        let out = render_errors(&snap);
        assert!(
            out.contains("No failures in this live runtime projection"),
            "empty errors should state the bounded evidence scope: {out}"
        );
    }

    #[test]
    fn render_errors_shows_tool_and_category() {
        let snap = IntrospectSnapshot {
            tool_errors: vec![ToolErrorEntry {
                tool: "bash".into(),
                signature_hint: "bash:ls -la".into(),
                failure_category: Some("Timeout".into()),
                error_preview: Some("command timed out".into()),
                at_epoch: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
                error_message: "command timed out".into(),
                file_path: None,
                file_range: None,
                turn: 3,
                round: 1,
            }],
            ..Default::default()
        };
        let out = render_errors(&snap);
        assert!(out.contains("bash"), "tool name missing: {out}");
        assert!(out.contains("Timeout"), "category missing: {out}");
        assert!(out.contains("command timed out"), "preview missing: {out}");
        assert!(
            out.contains("Signature hints"),
            "hints section missing: {out}"
        );
        assert!(out.contains("bash:ls -la"), "signature hint missing: {out}");
    }

    #[test]
    fn render_errors_truncates_preview_to_80_chars() {
        let long_preview = "x".repeat(200);
        let snap = IntrospectSnapshot {
            tool_errors: vec![ToolErrorEntry {
                tool: "read_file".into(),
                signature_hint: "read_file:/long/path".into(),
                failure_category: None,
                error_preview: Some(long_preview.clone()),
                at_epoch: 1000,
                error_message: long_preview.clone(),
                file_path: Some("/long/path/file.txt".into()),
                file_range: Some("200-400".into()),
                turn: 10,
                round: 3,
            }],
            ..Default::default()
        };
        let out = render_errors(&snap);
        // The rendered preview in the table should be at most 80 chars
        assert!(
            !out.contains(&long_preview),
            "full 200-char preview should not appear in render"
        );
    }

    // ── Tests for circuit breaker rendering ──────────────────────────────

    #[test]
    fn render_stall_with_circuit_breaker() {
        let snap = IntrospectSnapshot {
            circuit_breaker: Some(CircuitBreakerSnapshot {
                state: "recovering".into(),
                failure_count: 5,
                success_count: 20,
                consecutive_failures: 3,
            }),
            ..Default::default()
        };
        let out = render_stall_state(&snap);
        assert!(out.contains("recovering"), "CB state missing: {out}");
        assert!(
            out.contains("consecutive_failures=3"),
            "CB consecutive missing: {out}"
        );
    }

    // ── Tests for enhanced tool health rendering ─────────────────────────

    #[test]
    fn render_full_shows_health_avoidance_tool() {
        let snap = sample_snapshot();
        let out = render_full(&snap);
        assert!(out.contains("YES"), "health avoidance YES missing: {out}");
        assert!(
            out.contains("Timeout"),
            "last failure category missing: {out}"
        );
        assert!(out.contains("ConsecFail"), "header missing: {out}");
    }

    // ── Unhappy-path tests ───────────────────────────────────────────────

    #[test]
    fn render_json_empty_snapshot_per_facet_no_crash() {
        let empty = IntrospectSnapshot::default();
        for facet_str in &["session", "recent", "volatile", "stall", "noise", "errors"] {
            let req = IntrospectRequest::from_args(&serde_json::json!({
                "facet": facet_str,
                "format": "json"
            }));
            let out = render_introspect_request(&empty, &req);
            let report: IntrospectReport = serde_json::from_str(&out).unwrap_or_else(|e| {
                panic!("facet={facet_str}: JSON deserialize failed: {e}: {out}")
            });
            assert!(
                !report.observations.iter().any(|o| o.severity == "critical"),
                "facet={facet_str}: empty snapshot should not produce critical observations"
            );
            assert_report_refs_are_valid(&report);
        }
    }

    #[test]
    fn render_json_budget_priority_warnings_survive() {
        let mut snap = sample_snapshot();
        snap.alerts.clear();
        snap.tool_health = (0..5)
            .map(|idx| ToolHealthEntry {
                name: format!("tool_{idx}"),
                calls: 10,
                errors: 3,
                input_validation_failures: 0,
                avg_ms: 100,
                avoidance_advised: true,
                consecutive_failures: 3,
                last_failure_category: Some("timeout".into()),
            })
            .collect();
        snap.tool_health.push(ToolHealthEntry {
            name: "clean_tool".into(),
            calls: 5,
            errors: 0,
            input_validation_failures: 0,
            avg_ms: 5,
            avoidance_advised: false,
            consecutive_failures: 0,
            last_failure_category: None,
        });

        let req = IntrospectRequest::from_args(&serde_json::json!({
            "depth": "summary",
            "format": "json"
        }));
        let out = render_introspect_request(&snap, &req);
        let report: IntrospectReport = serde_json::from_str(&out).expect("json report");

        // 1 health + 5 warning tool_health = 6 observations. Budget=8: fits.
        // Action hints budget=4, but we have 5 → truncated flag set.
        assert_eq!(report.observations.len(), 6);
        assert_eq!(report.action_hints.len(), 4);
        assert!(report.budget_result.truncated);
        assert_eq!(report.budget_result.omitted.action_hints, 1);
        let warning_obs: Vec<_> = report
            .observations
            .iter()
            .filter(|o| o.severity == "warning")
            .collect();
        assert_eq!(
            warning_obs.len(),
            5,
            "all warning tool_health observations should survive"
        );
        assert_report_refs_are_valid(&report);
    }

    #[test]
    fn render_json_budget_truncation_drops_info_before_warning() {
        let mut snap = sample_snapshot();
        // sample_snapshot already has: bash (errors=5), read_file (all=0, filtered), grep (avoidance=true)
        // Set alerts to avoid alert observation (would be warning)
        snap.alerts.clear();
        // Add 2 info + 4 warning tools = 6 new tools
        // Total tools passing filter: bash + grep + 2 info + 4 warning = 8 (fits take(8))
        // Observations: 1 health + 8 tool = 9 → budget=8 drops 1
        for idx in 0..2 {
            snap.tool_health.push(ToolHealthEntry {
                name: format!("info_tool_{idx}"),
                calls: 10,
                errors: 1,
                input_validation_failures: 0,
                avg_ms: 100,
                avoidance_advised: false,
                consecutive_failures: 1,
                last_failure_category: None,
            });
        }
        for idx in 0..4 {
            snap.tool_health.push(ToolHealthEntry {
                name: format!("bad_tool_{idx}"),
                calls: 10,
                errors: 4,
                input_validation_failures: 0,
                avg_ms: 100,
                avoidance_advised: true,
                consecutive_failures: 3,
                last_failure_category: Some("timeout".into()),
            });
        }

        let req = IntrospectRequest::from_args(&serde_json::json!({
            "depth": "summary",
            "format": "json"
        }));
        let out = render_introspect_request(&snap, &req);
        let report: IntrospectReport = serde_json::from_str(&out).expect("json report");

        // 1 health + 8 tool = 9 observations. Budget=8, priority sort drops health (info).
        // Remaining: 5 warnings (grep+4bad) + 3 info (bash+2info) = 8
        assert_eq!(
            report.observations.len(),
            8,
            "should hit summary budget of 8"
        );
        assert!(report.budget_result.truncated);
        let retained_warnings: Vec<_> = report
            .observations
            .iter()
            .filter(|o| o.severity == "warning")
            .collect();
        assert_eq!(
            retained_warnings.len(),
            5,
            "all 5 warning observations must survive, only info gets truncated"
        );
        assert!(report.budget_result.omitted.observations >= 1);
        assert_report_refs_are_valid(&report);
    }

    #[test]
    fn render_json_tool_errors_facet_isolated_from_tool_health() {
        let mut snap = sample_snapshot();
        snap.tool_errors.push(ToolErrorEntry {
            tool: "bash".into(),
            signature_hint: "bash:ls -la".into(),
            failure_category: Some("tool_timeout".into()),
            error_preview: Some("command timed out after 30s".into()),
            at_epoch: 1,
            error_message: "command timed out after 30s".into(),
            file_path: None,
            file_range: None,
            turn: 5,
            round: 2,
        });

        let req = IntrospectRequest::from_args(&serde_json::json!({
            "facet": "errors",
            "format": "json"
        }));
        let out = render_introspect_request(&snap, &req);
        let report: IntrospectReport = serde_json::from_str(&out).expect("json report");

        assert_eq!(report.facet, "errors");
        assert!(
            !report.observations.iter().any(|o| o.kind == "tool_health"),
            "errors facet must not contain tool_health observations"
        );
        assert!(
            report
                .observations
                .iter()
                .any(|o| o.kind == "tool_error:tool_timeout"),
            "errors facet must contain tool_error observations"
        );
        assert_report_refs_are_valid(&report);
    }
}
