use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use astra_core::{
    ObservationActionHint, ObservationBudgetOmitted, ObservationBudgetResult,
    ObservationConfidence, ObservationDataCoverage, ObservationEvidence, ObservationFailureCluster,
    ObservationGraphEdgeKind, ObservationGraphLayer, ObservationGraphNode,
    ObservationGraphNodeKind, ObservationGraphSlice, ObservationProviderCoverage,
    ObservationRecord, ObservationView, SourcePolicy, Urn, classify_event_kind, push_graph_edge,
    push_graph_node, truncate_graph_summary,
};

use super::{
    IntrospectRequest, IntrospectSnapshot, ObservationFacet, prompt_cache_read_share_pct,
    turn_budget_label,
};

const RUNTIME_SNAPSHOT_REF: &str = "urn:astra:context:local:introspect:runtime_snapshot";
const INVOCATION_LIFECYCLE_REF: &str = "urn:astra:evidence:durable:introspect:invocation_lifecycle";
pub const INTROSPECT_REPORT_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IntrospectReport {
    pub schema_version: u32,
    pub tool: String,
    pub topic: String,
    pub facet: String,
    pub depth: String,
    pub horizon: String,
    pub source_policy: String,
    pub include_context: bool,
    /// Stable identity of the evidence state represented by this snapshot.
    /// Acquisition metadata (round counters, latency, and the observation
    /// calls themselves) is intentionally excluded so a diagnostic read does
    /// not manufacture a new task-evidence revision.
    #[serde(default)]
    pub evidence_revision: String,
    /// Facets whose evidence is represented by this bounded report. This is
    /// separate from the revision: a different facet can add useful coverage
    /// without the underlying runtime evidence changing.
    #[serde(default)]
    pub covered_facets: Vec<String>,
    pub data_coverage: ObservationDataCoverage,
    pub summary: String,
    /// Canonical bounded runtime facts for machine consumers such as the
    /// Desktop. Text summaries are projections of this frame, never a second
    /// authority that clients must parse.
    pub runtime_feedback: Option<crate::context_feedback::RuntimeFeedbackFrame>,
    /// Session-scoped ledger facts read during this diagnostic, not final
    /// totals for the live turn. Detail and omission counts follow depth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judgment_usage: Option<super::JudgmentUsageSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_judgments:
        Option<astra_services::semantic_judgment_observation::SemanticJudgmentView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_judgments:
        Option<astra_services::tool_result_selection_observation::ToolResultJudgmentView>,
    pub view: ObservationView,
    #[serde(default)]
    pub observations: Vec<ObservationRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<ObservationEvidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub action_hints: Vec<ObservationActionHint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_clusters: Vec<ObservationFailureCluster>,
    #[serde(default)]
    pub graph_slice: ObservationGraphSlice,
    #[serde(default)]
    pub budget_result: ObservationBudgetResult,
}

pub fn build_introspect_report(
    snapshot: &IntrospectSnapshot,
    request: &IntrospectRequest,
) -> IntrospectReport {
    if matches!(
        request.horizon,
        astra_core::ObservationHorizon::Turn
            | astra_core::ObservationHorizon::Session
            | astra_core::ObservationHorizon::CrossSession
    ) {
        let requested_horizon = request.horizon.as_str();
        let mut live_request = request.clone();
        live_request.horizon = astra_core::ObservationHorizon::Recent;
        let mut report = build_introspect_report(snapshot, &live_request);
        let warning = format!(
            "requested historical horizon={requested_horizon}; introspect returned the recent live projection only; use reflect for persisted causal evidence"
        );
        report.data_coverage.overall = "partial".to_string();
        report.data_coverage.warnings.push(warning.clone());
        report.view.data_coverage = report.data_coverage.clone();
        report.summary = format!("{warning}. {}", report.summary);
        return report;
    }
    if matches!(
        request.facet,
        ObservationFacet::Cache | ObservationFacet::SessionMemory
    ) {
        return build_edge_local_unavailable_report(request);
    }

    let mut warnings = Vec::new();
    if request.include_context {
        warnings.push(
            "include_context requested, but this runtime snapshot renderer has no visible-context provider"
                .to_string(),
        );
    }
    let judgment_usage = super::judgment_usage_view(snapshot, request);
    if let Some(usage) = &judgment_usage
        && (usage.coverage != super::JudgmentUsageCoverage::Available
            || usage
                .attempts_without_complete_usage
                .is_some_and(|count| count > 0))
    {
        warnings.push(format!(
            "judgment physical usage coverage={:?}; missing usage is unknown, not zero",
            usage.coverage
        ));
    }
    let semantic_judgments = super::semantic_judgment_view(snapshot, request);
    if let Some(semantics) = &semantic_judgments {
        warnings.push(format!(
            "semantic judgment trace coverage={:?}; capture incomplete; model adoption unknown",
            semantics.coverage
        ));
    }
    let tool_result_judgments = super::tool_result_judgment_view(snapshot, request);
    if let Some(judgments) = &tool_result_judgments
        && (judgments.evaluation_coverage
            != astra_services::tool_result_selection_observation::ToolResultJudgmentCoverage::NotObserved
            || judgments.application_coverage
                != astra_services::tool_result_selection_observation::ToolResultJudgmentCoverage::NotObserved)
    {
        warnings.push(format!(
            "tool-result judgment evaluation coverage={:?}; application coverage={:?}",
            judgments.evaluation_coverage, judgments.application_coverage
        ));
    }
    let data_coverage = introspect_data_coverage(snapshot, request, warnings);
    let evidence_revision = evidence_revision(snapshot, request);
    let view = ObservationView {
        topic: request.topic.as_str().to_string(),
        facet: request.facet.as_str().to_string(),
        depth: request.depth.as_str().to_string(),
        horizon: request.horizon.as_str().to_string(),
        data_coverage: data_coverage.clone(),
    };

    let summary = format!(
        "snapshot_cutoff=before_current_introspect_execution; later calls are absent. {}",
        introspect_summary(snapshot, request),
    );
    let mut observations = build_introspect_observations(snapshot, request, &summary);

    let runtime_summary = snapshot.runtime_feedback.as_ref().map_or_else(
        || "runtime_feedback=not_yet_observed".to_string(),
        |frame| {
            let pressure = frame.context.token_pressure.map_or_else(
                || "unknown".to_string(),
                |value| format!("{:.0}%", value * 100.0),
            );
            let cache_share = prompt_cache_read_share_pct(snapshot).map_or_else(
                || "unknown".to_string(),
                |value| format!("{value:.0}%"),
            );
            let (input_total, cached_read, cache_create) = frame.run_usage.map_or_else(
                || ("unknown".into(), "unknown".into(), "unknown".into()),
                |usage| (
                    usage.total_input().to_string(),
                    usage.cache_read.to_string(),
                    usage.cache_creation.to_string(),
                ),
            );
            format!(
                "snapshot_cutoff=before_current_introspect_execution pressure={} prompt_cache_read_share={} prompt_cache_scope=current_runtime_snapshot input_total={} cached_read={} cache_create={} turns={} signals={} tool_failures={}{}",
                pressure,
                cache_share,
                input_total,
                cached_read,
                cache_create,
                turn_budget_label(snapshot),
                snapshot.alerts.len(),
                snapshot.tool_errors.len(),
                snapshot_age_suffix(snapshot),
            )
        },
    );
    let mut evidence = vec![ObservationEvidence {
        ref_id: RUNTIME_SNAPSHOT_REF.to_string(),
        evidence_class: "observed_evidence".to_string(),
        source: "runtime.introspect_snapshot".to_string(),
        summary: runtime_summary,
        confidence: ObservationConfidence::evidence(0.75),
    }];
    if let Some(semantics) = &semantic_judgments
        && semantics.counts.is_some()
    {
        observations.push(ObservationRecord {
            ref_id: "urn:astra:observation:local:introspect:semantic_judgments".into(),
            topic: request.topic.as_str().into(),
            facet: request.facet.as_str().into(),
            kind: "semantic_judgment_trace".into(),
            severity: "info".into(),
            summary: semantics.render_for_depth(request.depth),
            // Coverage and captured stages do not establish outcome confidence.
            confidence: ObservationConfidence {
                classification: None,
                evidence: None,
                causal: None,
            },
            evidence_refs: vec![RUNTIME_SNAPSHOT_REF.into()],
        });
        evidence[0].summary.push_str(match semantics.scope {
            astra_services::semantic_judgment_observation::SemanticJudgmentScope::LocalJournalAtRead => "\nsource=owner_local_journal.trace_span; bounded historical capture; model adoption unknown.",
            _ => "\nsource=agent_events.trace_span; owner/session-scoped captured semantic facts at read time; trace capture incomplete; model adoption unknown.",
        });
    }
    if let Some(usage) = &judgment_usage {
        if usage.coverage != super::JudgmentUsageCoverage::NotObserved {
            observations.push(ObservationRecord {
                ref_id: "urn:astra:observation:local:introspect:judgment_usage".into(),
                topic: request.topic.as_str().into(),
                facet: request.facet.as_str().into(),
                kind: "judgment_physical_usage".into(),
                severity: "info".into(),
                summary: usage.render_for_depth(request.depth),
                confidence: ObservationConfidence::evidence(1.0),
                evidence_refs: vec![RUNTIME_SNAPSHOT_REF.into()],
            });
        }
        // Hint retains only one evidence unit. Keep the ledger provenance and
        // bounded detail in the snapshot unit so its references remain valid.
        evidence[0].summary.push_str(if usage.scope.is_local() {
            "\nsource=owner_local_explain_capture; "
        } else {
            "\nsource=inference_provider_attempts; "
        });
        // Keep the shared evidence unit compact even for forensic reports;
        // the structured observation carries the bounded detail separately.
        evidence[0].summary.push_str(&usage.render_compact());
    }
    if let Some(judgments) = &tool_result_judgments
        && (judgments.evaluation_coverage
            != astra_services::tool_result_selection_observation::ToolResultJudgmentCoverage::NotObserved
            || judgments.application_coverage
                != astra_services::tool_result_selection_observation::ToolResultJudgmentCoverage::NotObserved)
    {
        observations.push(ObservationRecord {
            ref_id: "urn:astra:observation:local:introspect:tool_result_judgments".into(),
            topic: request.topic.as_str().into(),
            facet: request.facet.as_str().into(),
            kind: "tool_result_judgment".into(),
            severity: "info".into(),
            summary: judgments.render_for_depth(request.depth),
            confidence: ObservationConfidence::evidence(1.0),
            evidence_refs: vec![RUNTIME_SNAPSHOT_REF.into()],
        });
    }
    if let Some(lifecycle) = snapshot.invocation_lifecycle.as_ref() {
        evidence.push(ObservationEvidence {
            ref_id: INVOCATION_LIFECYCLE_REF.to_string(),
            evidence_class: "durable_state_evidence".to_string(),
            source: "matrixone.tool_invocation_lifecycle".to_string(),
            summary: format!(
                "hot={} prepared={} dispatched={} unknown={} archives={} artifact_refs={} reconciliations={} deferred={}",
                lifecycle.hot_total,
                lifecycle.prepared,
                lifecycle.dispatched,
                lifecycle.outcome_unknown,
                lifecycle.archive_chunks,
                lifecycle.durable_artifact_references,
                lifecycle.reconciliation_events,
                lifecycle.compaction_deferred_events,
            ),
            confidence: ObservationConfidence::evidence(0.99),
        });
    }

    let mut action_hints = build_introspect_action_hints(snapshot, &observations);
    let budget_result =
        apply_report_budget(request, &mut observations, &mut evidence, &mut action_hints);
    let covered_facets = delivered_covered_facets(request, &budget_result);

    let graph_slice =
        build_introspect_graph_slice(snapshot, request, &observations, &evidence, &action_hints);

    IntrospectReport {
        schema_version: INTROSPECT_REPORT_SCHEMA_VERSION,
        tool: "introspect".to_string(),
        topic: request.topic.as_str().to_string(),
        facet: request.facet.as_str().to_string(),
        depth: request.depth.as_str().to_string(),
        horizon: request.horizon.as_str().to_string(),
        source_policy: request.source_policy.as_str().to_string(),
        include_context: request.include_context,
        evidence_revision,
        covered_facets,
        data_coverage,
        summary,
        runtime_feedback: snapshot.runtime_feedback.clone(),
        judgment_usage,
        semantic_judgments,
        tool_result_judgments,
        view,
        observations,
        evidence,
        action_hints,
        failure_clusters: Vec::new(),
        graph_slice,
        budget_result,
    }
}

fn build_edge_local_unavailable_report(request: &IntrospectRequest) -> IntrospectReport {
    let facet = request.facet.as_str();
    let (reason, warning) = if request.source_policy.allows_edge_local_artifacts() {
        (
            "no CLI/Edge-local artifact provider is attached",
            format!(
                "CLI/Edge-local session artifacts for facet={facet} are unavailable in this runtime"
            ),
        )
    } else {
        (
            "source_policy_excludes_edge_local_artifacts",
            format!(
                "requested source_policy={} does not allow CLI/Edge-local artifacts for facet={facet}",
                request.source_policy.as_str()
            ),
        )
    };
    let provider_name = match request.facet {
        ObservationFacet::Cache => "local_cache_captures",
        ObservationFacet::SessionMemory => "local_journal",
        _ => "edge_local_artifacts",
    };

    let mut providers = BTreeMap::new();
    providers.insert(
        provider_name.to_string(),
        ObservationProviderCoverage {
            status: "missing".to_string(),
            freshness_ms: None,
            reason: Some(reason.to_string()),
        },
    );
    let data_coverage = ObservationDataCoverage {
        overall: "unavailable".to_string(),
        source: "edge_local_artifacts_unavailable".to_string(),
        events: 0,
        decisions: 0,
        providers,
        warnings: vec![warning],
    };
    let view = ObservationView {
        topic: request.topic.as_str().to_string(),
        facet: facet.to_string(),
        depth: request.depth.as_str().to_string(),
        horizon: request.horizon.as_str().to_string(),
        data_coverage: data_coverage.clone(),
    };
    let summary = format!(
        "Introspect unavailable for facet={facet}: {}",
        if request.source_policy.allows_edge_local_artifacts() {
            "no CLI/Edge-local artifact provider is attached"
        } else {
            "requested source_policy does not allow CLI/Edge-local artifacts"
        }
    );
    let observations = vec![ObservationRecord {
        ref_id: Urn::new("observation", "local", "introspect")
            .seg("data_surface")
            .seg(facet)
            .build(),
        topic: request.topic.as_str().to_string(),
        facet: facet.to_string(),
        kind: "data_surface_unavailable".to_string(),
        severity: "info".to_string(),
        summary: summary.clone(),
        confidence: ObservationConfidence::classification_evidence(1.0, 1.0),
        evidence_refs: Vec::new(),
    }];

    IntrospectReport {
        schema_version: INTROSPECT_REPORT_SCHEMA_VERSION,
        tool: "introspect".to_string(),
        topic: request.topic.as_str().to_string(),
        facet: facet.to_string(),
        depth: request.depth.as_str().to_string(),
        horizon: request.horizon.as_str().to_string(),
        source_policy: request.source_policy.as_str().to_string(),
        include_context: request.include_context,
        evidence_revision: "v1:unavailable".to_string(),
        // An unavailable provider did not deliver task evidence. The
        // requested facet is scope metadata, not proof of coverage.
        covered_facets: Vec::new(),
        data_coverage,
        summary,
        runtime_feedback: None,
        judgment_usage: None,
        semantic_judgments: None,
        tool_result_judgments: None,
        view,
        observations,
        evidence: Vec::new(),
        action_hints: Vec::new(),
        failure_clusters: Vec::new(),
        graph_slice: ObservationGraphSlice::default(),
        budget_result: ObservationBudgetResult::default(),
    }
}

/// Compute a source-owned revision for the facts represented by the live
/// runtime snapshot. This is deliberately not a hash of the rendered result:
/// rendered text contains acquisition metadata such as current provider
/// counters, latency, and diagnostic activity. Those fields are useful to
/// inspect, but they are not new task evidence. The revision is shared across
/// facets; `covered_facets` records what a particular read actually delivered.
pub(crate) fn evidence_revision(
    snapshot: &IntrospectSnapshot,
    request: &IntrospectRequest,
) -> String {
    let material = json!({
        // Observer-generated advisory counters are not task evidence. The
        // underlying stall/admission facts remain represented below.
        "alerts": snapshot
            .alerts
            .iter()
            .filter(|alert| !alert.starts_with("advisory_signals:"))
            .collect::<Vec<_>>(),
        "tool_health": snapshot
            .tool_health
            .iter()
            .filter(|tool| !observation_tool(&tool.name))
            .collect::<Vec<_>>(),
        "working_memory_summary": snapshot.working_memory_summary,
        "lifecycle_summary": snapshot.lifecycle_summary,
        "capacity_provider_coverage": snapshot.capacity_provider_coverage,
        "tool_admission": snapshot.tool_admission,
        "semantic_cache_decisions": snapshot.semantic_cache_decisions,
        // These are observer feedback generated by the runtime itself. The
        // underlying work fact remains represented when it is task evidence.
        "volatile_pending": snapshot
            .volatile_pending
            .iter()
            .filter(|entry| !observer_feedback_kind(&entry.kind))
            .map(|entry| json!({"kind": entry.kind, "content": entry.content}))
            .collect::<Vec<_>>(),
        "stall_state": {
            "events": snapshot.stall_state.events,
            "advisory_signals": snapshot.stall_state.advisory_signals,
        },
        "circuit_breaker": snapshot.circuit_breaker.as_ref().map(|breaker| {
            // The host currently exposes a read-only streak through the
            // consecutive-failures field. A state transition is evidence;
            // the changing streak/counter is acquisition telemetry.
            json!({"state": breaker.state})
        }),
        "injection_freshness": snapshot
            .injection_freshness
            .iter()
            .map(|entry| {
                let status = match &entry.status {
                    crate::injection_tracking::ChannelStatus::Untracked => "untracked",
                    crate::injection_tracking::ChannelStatus::Empty { .. } => "empty",
                    crate::injection_tracking::ChannelStatus::Fresh { .. } => "fresh",
                    crate::injection_tracking::ChannelStatus::Stale { .. } => "stale",
                };
                json!({
                    "channel": entry.channel,
                    "status": status,
                    "preview": entry.preview,
                })
            })
            .collect::<Vec<_>>(),
        "recent_rounds": non_observation_rounds(snapshot),
        "step_latency": non_observation_steps(snapshot),
        // A failed reflect/introspect call is a real diagnostic fact. A
        // successful observation has no tool-error entry and therefore does
        // not churn this revision.
        "tool_errors": snapshot.tool_errors,
        // Successful observer dispatch/completion changes aggregate counters
        // on every read. Only terminal anomalies and reconciliation gaps are
        // evidence for convergence; ordinary observer lifecycle churn is not.
        "invocation_lifecycle": snapshot
            .invocation_lifecycle
            .as_ref()
            .map(invocation_lifecycle_evidence),
        // Normal runtime reads expose judgment state in the report, but
        // ordinary successful auxiliary attempts are observer telemetry: if
        // they changed the source revision on every round, Jev would make its
        // own introspection look perpetually new. Forensic judgment reads opt
        // into the full auxiliary ledger and therefore intentionally advance
        // on newly captured attempts/results.
        "auxiliary_evidence": auxiliary_evidence(snapshot, request),
    });

    let encoded = serde_json::to_vec(&material).unwrap_or_default();
    let digest = Sha256::digest(&encoded);
    format!("v1:{digest:x}")
}

fn invocation_lifecycle_evidence(
    lifecycle: &crate::introspect::InvocationLifecycleSnapshot,
) -> serde_json::Value {
    json!({
        "failed": lifecycle.failed,
        "rejected": lifecycle.rejected,
        "outcome_unknown": lifecycle.outcome_unknown,
        "rejected_without_dispatch": lifecycle.rejected_without_dispatch,
        "reconciliation_events": lifecycle.reconciliation_events,
        "compaction_deferred_events": lifecycle.compaction_deferred_events,
    })
}

fn auxiliary_evidence(snapshot: &IntrospectSnapshot, request: &IntrospectRequest) -> Value {
    if !matches!(
        request.facet,
        ObservationFacet::Overview
            | ObservationFacet::Session
            | ObservationFacet::Recent
            | ObservationFacet::Trace
    ) {
        return Value::Null;
    }
    if matches!(request.depth, astra_core::ObservationDepth::Forensic) {
        return json!({
            "judgment_usage": snapshot.judgment_usage,
            "semantic_judgments": snapshot.semantic_judgments,
            "tool_result_judgments": snapshot.tool_result_judgments,
        });
    }
    json!({
        "judgment_usage": snapshot.judgment_usage.as_ref().map(stable_judgment_usage),
        "semantic_judgments": snapshot
            .semantic_judgments
            .as_ref()
            .map(stable_semantic_judgments),
        "tool_result_judgments": snapshot
            .tool_result_judgments
            .as_ref()
            .map(stable_tool_result_judgments),
    })
}

fn stable_judgment_usage(usage: &super::JudgmentUsageSnapshot) -> Value {
    json!({
        "scope": usage.scope,
        "capture_incomplete": usage.capture_incomplete,
        "coverage": usage.coverage,
        "input_complete": usage.input_complete,
        "output_complete": usage.output_complete,
    })
}

fn stable_semantic_judgments(
    judgments: &astra_services::semantic_judgment_observation::SemanticJudgmentView,
) -> Value {
    let counts = judgments.counts.as_ref().map(|counts| {
        json!({
            "abstained": counts.abstained,
            "invalid": counts.invalid,
            "not_dispatched": counts.not_dispatched,
            "evaluation_unavailable": counts.evaluation_unavailable,
            "conflicting": counts.conflicting,
        })
    });
    json!({
        "scope": judgments.scope,
        "coverage": judgments.coverage,
        "capture_incomplete": judgments.capture_incomplete,
        "capture_truncated": judgments.capture_truncated,
        "adoption": "unknown",
        "counts": counts,
        "decisions": judgments.decision_summaries(),
        "capture_gaps": judgments.capture_gaps,
    })
}

fn stable_tool_result_judgments(
    judgments: &astra_services::tool_result_selection_observation::ToolResultJudgmentView,
) -> Value {
    json!({
        "evaluation_coverage": judgments.evaluation_coverage,
        "application_coverage": judgments.application_coverage,
        "terminal_missing": judgments.terminal_missing,
        "conflicting": judgments.conflicting,
        "invalid_or_oversized": judgments.invalid_or_oversized,
        "applications": {
            "unknown": judgments.applications.unknown,
            "invalid_or_conflicting": judgments.applications.invalid_or_conflicting,
        },
    })
}

/// Facets actually represented by a structured report. A bounded/truncated
/// report cannot prove that a facet was fully delivered, so it contributes no
/// cross-facet reuse marker. JSON overview is intentionally scoped to the
/// composite overview itself: its summary names other facets, but it does not
/// deliver their bounded detail.
pub(crate) fn delivered_covered_facets(
    request: &IntrospectRequest,
    budget_result: &ObservationBudgetResult,
) -> Vec<String> {
    if budget_result.truncated {
        return Vec::new();
    }
    if request.format.is_json() && matches!(request.facet, ObservationFacet::Overview) {
        return vec!["overview".to_string()];
    }
    match request.facet {
        ObservationFacet::Overview => [
            "session", "recent", "errors", "trace", "volatile", "stall", "noise",
        ]
        .into_iter()
        .map(str::to_string)
        .collect(),
        facet => vec![facet.as_str().to_string()],
    }
}

/// Text rendering has one composite body for overview, but the final tool
/// result may still be bounded by a downstream sanitizer. Keep the marker
/// conservative so a front-loaded boundary cannot claim every nested facet
/// after the body was cut.
pub(crate) fn text_covered_facets(request: &IntrospectRequest) -> Vec<String> {
    if matches!(
        request.facet,
        ObservationFacet::Cache | ObservationFacet::SessionMemory
    ) {
        return Vec::new();
    }
    match request.facet {
        ObservationFacet::Overview => vec!["overview".to_string()],
        facet => vec![facet.as_str().to_string()],
    }
}

fn observation_tool(name: &str) -> bool {
    matches!(name, "introspect" | "reflect")
}

fn observer_feedback_kind(kind: &str) -> bool {
    matches!(
        kind,
        "CircuitBreaker" | "PolicyAdvisory" | "BehaviorAdvisory" | "ToolBatchCoaching"
    )
}

fn non_observation_rounds(
    snapshot: &IntrospectSnapshot,
) -> Vec<&crate::introspect::RoundSnapshotEntry> {
    snapshot
        .recent_rounds
        .iter()
        .filter(|round| {
            !round.tool_call_names.is_empty()
                && round
                    .tool_call_names
                    .iter()
                    .any(|name| !observation_tool(name))
        })
        .collect()
}

fn non_observation_steps(
    snapshot: &IntrospectSnapshot,
) -> Vec<&crate::introspect::StepLatencySnapshotEntry> {
    snapshot
        .step_latency
        .iter()
        .filter(|step| {
            step.tool_call_count > 0
                && !step
                    .first_tool_name
                    .as_deref()
                    .is_some_and(observation_tool)
        })
        .collect()
}

pub(super) fn render_introspect_report_json(
    snapshot: &IntrospectSnapshot,
    request: &IntrospectRequest,
) -> String {
    serde_json::to_string(&build_introspect_report(snapshot, request)).unwrap_or_else(|error| {
        serde_json::json!({
            "summary": "failed to serialize introspect report",
            "error": error.to_string(),
        })
        .to_string()
    })
}

fn runtime_event_count(snapshot: &IntrospectSnapshot) -> i64 {
    snapshot
        .recent_rounds
        .len()
        .saturating_add(snapshot.tool_errors.len())
        .saturating_add(snapshot.stall_state.events.len())
        .saturating_add(snapshot.alerts.len())
        .saturating_add(snapshot.semantic_cache_decisions.len())
        .saturating_add(usize::from(snapshot.invocation_lifecycle.is_some())) as i64
}

fn introspect_data_coverage(
    snapshot: &IntrospectSnapshot,
    request: &IntrospectRequest,
    warnings: Vec<String>,
) -> ObservationDataCoverage {
    let source = match request.source_policy {
        SourcePolicy::Auto | SourcePolicy::LiveFirst => "live_runtime_snapshot".to_string(),
        SourcePolicy::LiveOnly => "live_only_runtime_snapshot".to_string(),
        SourcePolicy::DurableFirst => "durable_first_runtime_snapshot".to_string(),
        SourcePolicy::LocalOnly => "local_runtime_snapshot".to_string(),
        SourcePolicy::CloudOnly => "cloud_runtime_snapshot".to_string(),
    };
    let events = runtime_event_count(snapshot);

    let mut providers = BTreeMap::new();
    providers.insert(
        "live_runtime".to_string(),
        ObservationProviderCoverage {
            status: "fresh".to_string(),
            freshness_ms: Some(0),
            reason: None,
        },
    );
    if request.include_context {
        providers.insert(
            "visible_context".to_string(),
            ObservationProviderCoverage {
                status: "missing".to_string(),
                freshness_ms: None,
                reason: Some("provider_not_attached".to_string()),
            },
        );
    }
    if let Some(semantics) = super::semantic_judgment_view(snapshot, request) {
        providers.insert(
            "semantic_judgment_trace".into(),
            ObservationProviderCoverage {
                status: if semantics.counts.is_some() {
                    "partial"
                } else {
                    "missing"
                }
                .into(),
                freshness_ms: None,
                reason: Some(format!(
                    "session_trace_at_read:{:?};classification_not_execution_authority",
                    semantics.coverage
                )),
            },
        );
    }
    if let Some(judgments) = super::tool_result_judgment_view(snapshot, request) {
        use astra_services::tool_result_selection_observation::ToolResultJudgmentCoverage as Coverage;
        let missing = matches!(
            judgments.evaluation_coverage,
            Coverage::NotObserved | Coverage::SourceUnavailable | Coverage::SourceExcluded
        ) && matches!(
            judgments.application_coverage,
            Coverage::NotObserved | Coverage::SourceUnavailable | Coverage::SourceExcluded
        );
        let partial = matches!(
            judgments.evaluation_coverage,
            Coverage::CaptureIncomplete | Coverage::CaptureTruncated
        ) || matches!(
            judgments.application_coverage,
            Coverage::CaptureIncomplete | Coverage::CaptureTruncated
        );
        providers.insert(
            "tool_result_judgment".into(),
            ObservationProviderCoverage {
                status: if missing {
                    "missing"
                } else if partial {
                    "partial"
                } else {
                    "fresh"
                }
                .into(),
                freshness_ms: None,
                reason: Some(format!(
                    "evaluation={:?};application={:?};recommendation_not_adoption",
                    judgments.evaluation_coverage, judgments.application_coverage
                )),
            },
        );
    }
    if let Some(usage) = super::judgment_usage_view(snapshot, request) {
        providers.insert(
            "judgment_inference_ledger".into(),
            ObservationProviderCoverage {
                status: if usage.coverage == super::JudgmentUsageCoverage::CaptureTruncated {
                    "partial"
                } else if usage.coverage != super::JudgmentUsageCoverage::Available {
                    "missing"
                } else if usage.attempts_without_complete_usage.is_some_and(|n| n > 0) {
                    "partial"
                } else {
                    "fresh"
                }
                .into(),
                freshness_ms: None,
                reason: Some(format!(
                    "session_supported_judgment_operations_at_ledger_read:{:?}",
                    usage.coverage
                )),
            },
        );
    }
    if matches!(
        request.source_policy,
        SourcePolicy::CloudOnly | SourcePolicy::DurableFirst
    ) {
        providers.insert(
            "cloud_events".to_string(),
            ObservationProviderCoverage {
                status: "missing".to_string(),
                freshness_ms: None,
                reason: Some("not_available_in_runtime_snapshot".to_string()),
            },
        );
    }

    ObservationDataCoverage {
        overall: if !warnings.is_empty() {
            "partial".to_string()
        } else {
            "fresh".to_string()
        },
        source,
        events,
        decisions: snapshot.semantic_cache_decisions.len() as i64,
        providers,
        warnings,
    }
}

fn apply_report_budget(
    request: &IntrospectRequest,
    observations: &mut Vec<ObservationRecord>,
    evidence: &mut Vec<ObservationEvidence>,
    action_hints: &mut Vec<ObservationActionHint>,
) -> ObservationBudgetResult {
    let (max_observations, max_evidence, max_hints) = request.depth.report_limits();

    // Sort by priority before truncation so high-value observations survive.
    observations.sort_by_key(|o| std::cmp::Reverse(observation_priority_key(o)));

    let omitted_observations = truncate_by_priority(observations, max_observations);
    let retained_observation_refs = observations
        .iter()
        .map(|observation| observation.ref_id.as_str())
        .collect::<BTreeSet<_>>();
    let hints_before_ref_filter = action_hints.len();
    action_hints.iter_mut().for_each(|hint| {
        hint.observation_refs
            .retain(|ref_id| retained_observation_refs.contains(ref_id.as_str()));
    });
    action_hints.retain(|hint| !hint.observation_refs.is_empty());
    let omitted_dangling_hints = hints_before_ref_filter.saturating_sub(action_hints.len()) as i64;
    let omitted_evidence = truncate_by_priority(evidence, max_evidence);
    let omitted_hints = truncate_by_priority(action_hints, max_hints);
    ObservationBudgetResult {
        truncated: omitted_observations > 0
            || omitted_evidence > 0
            || omitted_hints > 0
            || omitted_dangling_hints > 0,
        next_cursor: None,
        omitted: ObservationBudgetOmitted {
            evidence_previews: omitted_evidence,
            observations: omitted_observations,
            action_hints: omitted_hints + omitted_dangling_hints,
            ..Default::default()
        },
    }
}

/// Priority key for sorting observations before budget truncation.
/// Higher = more important. warning > info; higher confidence > lower; system > detail.
pub(super) fn observation_priority_key(o: &ObservationRecord) -> i64 {
    let severity_score = match o.severity.as_str() {
        "critical" => 1000,
        "error" => 800,
        "warning" => 600,
        _ => 0,
    };
    let confidence_score = (o.confidence.evidence.unwrap_or(0.5)
        + o.confidence.classification.unwrap_or(0.0)
        + o.confidence.causal.unwrap_or(0.0))
        * 100.0;
    let kind_score =
        if o.kind.contains("alert") || o.kind.contains("stall") || o.kind.contains("failure") {
            200
        } else if o.kind.contains("health") || o.kind.contains("error") {
            100
        } else {
            0
        };
    severity_score + confidence_score as i64 + kind_score
}

fn truncate_by_priority<T>(items: &mut Vec<T>, max: usize) -> i64 {
    let omitted = items.len().saturating_sub(max) as i64;
    items.truncate(max);
    omitted
}

fn introspect_summary(snapshot: &IntrospectSnapshot, request: &IntrospectRequest) -> String {
    let summary = match request.facet {
        ObservationFacet::Errors => {
            if snapshot.tool_errors.is_empty() {
                "No recent tool errors recorded".to_string()
            } else {
                format!("{} recent tool errors recorded", snapshot.tool_errors.len())
            }
        }
        ObservationFacet::Stall => format!(
            "Stall guard: {} events, {} introspections, {} advisory signals",
            snapshot.stall_state.events.len(),
            snapshot.stall_state.introspection_count,
            snapshot.stall_state.advisory_signals.len(),
        ),
        _ if !snapshot.alerts.is_empty() => {
            format!(
                "Runtime snapshot has {} active alerts",
                snapshot.alerts.len()
            )
        }
        _ => snapshot.runtime_feedback.as_ref().map_or_else(
            || "Runtime feedback not yet observed".to_string(),
            |frame| {
                let pressure = frame.context.token_pressure.map_or_else(
                    || "unknown".to_string(),
                    |value| format!("{:.0}%", value * 100.0),
                );
                let cache_share = prompt_cache_read_share_pct(snapshot).map_or_else(
                    || "unknown".to_string(),
                    |value| format!("{value:.0}%"),
                );
                format!(
                    "Runtime snapshot healthy - pressure {}, prompt_cache_read_share {} (scope=current_runtime_snapshot), turns {}",
                    pressure,
                    cache_share,
                    turn_budget_label(snapshot),
                )
            },
        ),
    };
    if matches!(
        request.facet,
        ObservationFacet::Session | ObservationFacet::Overview
    ) {
        format!("{summary}; {}", composite_diagnostic_summary(snapshot))
    } else {
        summary
    }
}

/// Keep the first serialized part of an overview useful when the complete
/// composite report is moved behind a bounded artifact projection. The full
/// report remains available to structured consumers; this summary is the
/// model's first evidence boundary and explicitly names the facets it covers.
fn composite_diagnostic_summary(snapshot: &IntrospectSnapshot) -> String {
    let mut summary = format!(
        "composite_facets=runtime,recent,errors,trace,volatile,stall,noise recent_rounds={} tool_errors={} volatile_pending={} stall_events={} injection_channels={}",
        snapshot.recent_rounds.len(),
        snapshot.tool_errors.len(),
        snapshot.volatile_pending.len(),
        snapshot.stall_state.events.len(),
        snapshot.injection_freshness.len(),
    );

    if let Some(round) = snapshot.recent_rounds.last() {
        summary.push_str(&format!(
            " latest_round=t{}_r{} model={} in={} cache_read={} out={} tools={} duration_ms={} finish={}",
            round.turn,
            round.round,
            if round.model.is_empty() {
                round.provider.as_str()
            } else {
                round.model.as_str()
            },
            round.prompt_tokens,
            round.cache_read_tokens,
            round.completion_tokens,
            round.tool_call_names.len(),
            round.duration_ms,
            round.finish_reason.as_deref().unwrap_or("unknown"),
        ));
    }

    if snapshot.tool_errors.is_empty() {
        summary.push_str(" errors_live=none");
    } else {
        let samples = snapshot
            .tool_errors
            .iter()
            .take(3)
            .map(|error| {
                let preview = error
                    .error_preview
                    .as_deref()
                    .unwrap_or(&error.signature_hint)
                    .replace('\n', " ");
                format!(
                    "{}:{}",
                    error.tool,
                    preview.chars().take(120).collect::<String>()
                )
            })
            .collect::<Vec<_>>()
            .join(" | ");
        summary.push_str(" error_samples=");
        summary.push_str(&samples);
    }

    if let Some(step) = snapshot.step_latency.last() {
        summary.push_str(&format!(
            " latest_trace=step:{} dominant={} total_ms={} pre_tool_wait_ms={} tool_ms={} calls={} terminal={}",
            step.step_id,
            step.dominant_phase,
            step.total_ms
                .map_or_else(|| "unknown".to_string(), |value| value.to_string()),
            step.pre_tool_wait_ms
                .map_or_else(|| "unknown".to_string(), |value| value.to_string()),
            step.tool_execution_ms,
            step.tool_call_count,
            step.terminal_event_kind.as_deref().unwrap_or("unknown"),
        ));
    } else {
        summary.push_str(" latest_trace=unavailable");
    }

    summary.push_str(
        " overview_is_one_snapshot; use another facet only when this composite summary or its bounded report names a concrete missing detail",
    );
    summary
}

fn snapshot_age_suffix(snapshot: &IntrospectSnapshot) -> String {
    if snapshot.snapshot_age_turns == 0 {
        String::new()
    } else {
        format!(" snapshot_age_turns={}", snapshot.snapshot_age_turns)
    }
}

fn build_introspect_observations(
    snapshot: &IntrospectSnapshot,
    request: &IntrospectRequest,
    summary: &str,
) -> Vec<ObservationRecord> {
    let mut observations = Vec::new();

    // ── facet-specific observations ──
    match request.facet {
        ObservationFacet::Session | ObservationFacet::Overview => {
            // Session health observation
            if snapshot.alerts.is_empty() && snapshot.tool_errors.is_empty() {
                observations.push(ObservationRecord {
                    ref_id: "urn:astra:observation:local:introspect:runtime:health".to_string(),
                    topic: request.topic.as_str().to_string(),
                    facet: request.facet.as_str().to_string(),
                    kind: "runtime_health".to_string(),
                    severity: "info".to_string(),
                    summary: summary.to_string(),
                    confidence: ObservationConfidence::evidence(0.75),
                    evidence_refs: vec![RUNTIME_SNAPSHOT_REF.to_string()],
                });
            } else if !snapshot.alerts.is_empty() {
                observations.push(ObservationRecord {
                    ref_id: "urn:astra:observation:local:introspect:runtime:alerts".to_string(),
                    topic: request.topic.as_str().to_string(),
                    facet: request.facet.as_str().to_string(),
                    kind: "runtime_alert".to_string(),
                    severity: "warning".to_string(),
                    summary: format!("{} runtime alerts active", snapshot.alerts.len()),
                    confidence: ObservationConfidence::evidence(0.80),
                    evidence_refs: vec![RUNTIME_SNAPSHOT_REF.to_string()],
                });
            }

            // Tool health entries — only for Session/Overview facets
            for tool in snapshot
                .tool_health
                .iter()
                .filter(|tool| {
                    tool.avoidance_advised
                        || tool.errors > 0
                        || tool.input_validation_failures > 0
                        || tool.consecutive_failures > 0
                })
                .take(8)
            {
                observations.push(ObservationRecord {
                    ref_id: Urn::new("observation", "local", "introspect")
                        .seg("execution")
                        .seg("tool")
                        .seg(&tool.name)
                        .build(),
                    topic: "execution".to_string(),
                    facet: request.facet.as_str().to_string(),
                    kind: "tool_health".to_string(),
                    severity: if tool.avoidance_advised || tool.consecutive_failures >= 3 {
                        "warning"
                    } else {
                        "info"
                    }
                    .to_string(),
                    summary: format!(
                        "{} calls={} errors={} input_validation_failures={} consecutive_failures={}",
                        tool.name,
                        tool.calls,
                        tool.errors,
                        tool.input_validation_failures,
                        tool.consecutive_failures
                    ),
                    confidence: ObservationConfidence::complete(0.70, 0.75, 0.35),
                    evidence_refs: vec![RUNTIME_SNAPSHOT_REF.to_string()],
                });
            }

            for provider in &snapshot.capacity_provider_coverage {
                observations.push(ObservationRecord {
                    ref_id: Urn::new("observation", "local", "introspect")
                        .seg("capacity")
                        .seg(provider.provider_type.as_str())
                        .build(),
                    topic: "execution".to_string(),
                    facet: request.facet.as_str().to_string(),
                    kind: "capacity_provider".to_string(),
                    severity: if provider.status == "ready" {
                        "info"
                    } else {
                        "warning"
                    }
                    .to_string(),
                    summary: capacity_provider_observation_summary(provider),
                    confidence: ObservationConfidence::evidence(0.85),
                    evidence_refs: vec![RUNTIME_SNAPSHOT_REF.to_string()],
                });
            }
        }

        ObservationFacet::Errors => {
            if !snapshot.tool_errors.is_empty() {
                observations.push(ObservationRecord {
                    ref_id: "urn:astra:observation:local:introspect:errors:recent".to_string(),
                    topic: request.topic.as_str().to_string(),
                    facet: request.facet.as_str().to_string(),
                    kind: "tool_failure_cluster".to_string(),
                    severity: "warning".to_string(),
                    summary: format!("{} recent tool errors recorded", snapshot.tool_errors.len()),
                    confidence: ObservationConfidence::evidence(0.85),
                    evidence_refs: vec![RUNTIME_SNAPSHOT_REF.to_string()],
                });
            }

            // Tool error entries — only for Errors facet
            for (idx, error) in snapshot.tool_errors.iter().enumerate() {
                observations.push(ObservationRecord {
                    ref_id: Urn::new("observation", "local", "introspect")
                        .seg("execution")
                        .seg("error")
                        .idx(idx)
                        .build(),
                    topic: "execution".to_string(),
                    facet: request.facet.as_str().to_string(),
                    kind: error
                        .failure_category
                        .as_deref()
                        .map(|category| format!("tool_error:{category}"))
                        .unwrap_or_else(|| "tool_error".to_string()),
                    severity: "warning".to_string(),
                    summary: error
                        .error_preview
                        .clone()
                        .filter(|preview| !preview.trim().is_empty())
                        .unwrap_or_else(|| error.signature_hint.clone()),
                    confidence: ObservationConfidence::complete(0.75, 0.80, 0.45),
                    evidence_refs: vec![RUNTIME_SNAPSHOT_REF.to_string()],
                });
            }
        }

        ObservationFacet::Stall => {
            if !snapshot.stall_state.events.is_empty()
                || snapshot.stall_state.introspection_count > 0
            {
                observations.push(ObservationRecord {
                    ref_id: "urn:astra:observation:local:introspect:stall:state".to_string(),
                    topic: request.topic.as_str().to_string(),
                    facet: request.facet.as_str().to_string(),
                    kind: "stall_telemetry".to_string(),
                    severity: "info".to_string(),
                    summary: summary.to_string(),
                    confidence: ObservationConfidence::evidence(0.90),
                    evidence_refs: vec![RUNTIME_SNAPSHOT_REF.to_string()],
                });
            }
            for correction in &snapshot.stall_state.advisory_signals {
                observations.push(ObservationRecord {
                    ref_id: Urn::new("observation", "local", "introspect")
                        .seg("stall")
                        .seg("correction")
                        .seg(correction)
                        .build(),
                    topic: request.topic.as_str().to_string(),
                    facet: request.facet.as_str().to_string(),
                    kind: "stall_advisory_signal".to_string(),
                    severity: "warning".to_string(),
                    summary: format!("advisory signal emitted: {correction}"),
                    confidence: ObservationConfidence::evidence(0.95),
                    evidence_refs: vec![RUNTIME_SNAPSHOT_REF.to_string()],
                });
            }
        }

        _ => {}
    }

    if matches!(
        request.facet,
        ObservationFacet::Session | ObservationFacet::Overview | ObservationFacet::Trace
    ) {
        if let Some(lifecycle) = snapshot.invocation_lifecycle.as_ref() {
            let unhealthy = lifecycle.prepared > 0
                || lifecycle.dispatched > 0
                || lifecycle.outcome_unknown > 0
                || lifecycle.compaction_deferred_events > 0;
            observations.push(ObservationRecord {
                ref_id: Urn::new("observation", "durable", "introspect")
                    .seg("invocation_lifecycle")
                    .build(),
                topic: "execution".to_string(),
                facet: request.facet.as_str().to_string(),
                kind: "durable_invocation_lifecycle".to_string(),
                severity: if unhealthy { "warning" } else { "info" }.to_string(),
                summary: format!(
                    "hot={} prepared={} dispatched={} unknown={} archives={} artifact_refs={} reconciliations={} deferred={}",
                    lifecycle.hot_total,
                    lifecycle.prepared,
                    lifecycle.dispatched,
                    lifecycle.outcome_unknown,
                    lifecycle.archive_chunks,
                    lifecycle.durable_artifact_references,
                    lifecycle.reconciliation_events,
                    lifecycle.compaction_deferred_events,
                ),
                confidence: ObservationConfidence::evidence(0.99),
                evidence_refs: vec![INVOCATION_LIFECYCLE_REF.to_string()],
            });
        }
        for (index, decision) in snapshot.semantic_cache_decisions.iter().enumerate() {
            observations.push(ObservationRecord {
                ref_id: Urn::new("observation", "local", "introspect")
                    .seg("semantic_cache")
                    .idx(index)
                    .build(),
                topic: "execution".to_string(),
                facet: request.facet.as_str().to_string(),
                kind: "semantic_read_cache_decision".to_string(),
                severity: if matches!(
                    decision.state.as_str(),
                    "hit"
                        | "filled"
                        | "fill_claimed"
                        | "policy_disabled"
                        | "rollout_disabled"
                        | "freshness_unavailable"
                ) {
                    "info"
                } else {
                    "warning"
                }
                .to_string(),
                summary: format!(
                    "{} semantic_read_cache={}",
                    decision.tool_name, decision.state
                ),
                confidence: ObservationConfidence::evidence(0.95),
                evidence_refs: vec![RUNTIME_SNAPSHOT_REF.to_string()],
            });
        }
        for admission in &snapshot.tool_admission {
            observations.push(ObservationRecord {
                ref_id: Urn::new("observation", "local", "introspect")
                    .seg("tool_admission")
                    .seg(&admission.tool_name)
                    .build(),
                topic: "execution".to_string(),
                facet: request.facet.as_str().to_string(),
                kind: "tool_admission".to_string(),
                severity: if admission.visible { "info" } else { "warning" }.to_string(),
                summary: tool_admission_observation_summary(admission),
                confidence: ObservationConfidence::evidence(0.85),
                evidence_refs: vec![RUNTIME_SNAPSHOT_REF.to_string()],
            });
        }
    }

    if matches!(
        request.facet,
        ObservationFacet::Session | ObservationFacet::Overview | ObservationFacet::Trace
    ) && let Some(summary) = latest_step_latency_summary(snapshot)
    {
        observations.push(ObservationRecord {
            ref_id: "urn:astra:observation:local:introspect:execution:step_latency".to_string(),
            topic: "execution".to_string(),
            facet: request.facet.as_str().to_string(),
            kind: "step_latency".to_string(),
            severity: "info".to_string(),
            summary,
            confidence: ObservationConfidence::evidence(0.80),
            evidence_refs: vec![RUNTIME_SNAPSHOT_REF.to_string()],
        });
    }

    observations
}

fn capacity_provider_observation_summary(
    provider: &super::CapacityProviderCoverageEntry,
) -> String {
    let capabilities = if provider.capabilities.is_empty() {
        "none".to_string()
    } else {
        provider.capabilities.join(",")
    };
    format!(
        "{} provider_id={} capabilities={}",
        super::capacity_provider_coverage_entry_summary(provider),
        provider.provider_id,
        capabilities
    )
}

fn tool_admission_observation_summary(admission: &super::ToolAdmissionSnapshotEntry) -> String {
    let selected = admission.selected_offer_id.as_deref().unwrap_or("-");
    let hidden = admission.hidden_reason.as_deref().unwrap_or("-");
    let candidates = admission
        .candidates
        .iter()
        .map(|candidate| {
            format!(
                "{}:{}:{}",
                candidate.offer_id, candidate.reason, candidate.readiness
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{} visible={} selected={} route={} hidden={} candidates=[{}]",
        admission.tool_name,
        admission.visible,
        selected,
        admission.selected_route,
        hidden,
        candidates
    )
}

fn latest_step_latency_summary(snapshot: &IntrospectSnapshot) -> Option<String> {
    let latest = snapshot.step_latency.last()?;
    Some(format!(
        "latest_step={} dominant={} total_ms={} pre_tool_wait_ms={} tool_execution_ms={} max_tool_execution_ms={} calls={} skipped={} first_tool={} terminal={}",
        latest.step_id,
        latest.dominant_phase,
        fmt_opt_u64(latest.total_ms),
        fmt_opt_u64(latest.pre_tool_wait_ms),
        latest.tool_execution_ms,
        latest.max_tool_execution_ms,
        latest.tool_call_count,
        latest.skipped_tool_count,
        latest.first_tool_name.as_deref().unwrap_or("-"),
        latest.terminal_event_kind.as_deref().unwrap_or("-"),
    ))
}

fn fmt_opt_u64(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn build_introspect_action_hints(
    snapshot: &IntrospectSnapshot,
    observations: &[ObservationRecord],
) -> Vec<ObservationActionHint> {
    let mut hints: Vec<ObservationActionHint> = Vec::new();
    let evidence_ref = RUNTIME_SNAPSHOT_REF.to_string();

    // ── 1. Tool policy hints (existing) ──
    for tool in snapshot
        .tool_health
        .iter()
        .filter(|t| t.avoidance_advised)
        .take(5)
    {
        let tool_ref = Urn::new("observation", "local", "introspect")
            .seg("execution")
            .seg("tool")
            .seg(&tool.name)
            .build();
        let observation_refs: Vec<String> = observations
            .iter()
            .filter(|obs| obs.ref_id == tool_ref)
            .map(|obs| obs.ref_id.clone())
            .collect();
        if !observation_refs.is_empty() {
            hints.push(ObservationActionHint {
                target_type: "tool_policy".to_string(),
                summary: format!("Avoid or verify {} until health recovers", tool.name),
                confidence: ObservationConfidence::classification_evidence(0.70, 0.75),
                observation_refs,
            });
        }
    }

    // ── 3. Token pressure hint ──
    if let Some(pressure) = snapshot
        .runtime_feedback
        .as_ref()
        .and_then(|frame| frame.context.token_pressure)
        && pressure > 0.80
    {
        hints.push(ObservationActionHint {
            target_type: "pressure_mitigation".to_string(),
            summary: format!(
                "Token pressure {:.0}% — prefer targeted reads (line ranges) over full files; batch independent calls",
                pressure * 100.0
            ),
            confidence: ObservationConfidence::classification_evidence(0.60, 0.90),
            observation_refs: vec![evidence_ref.clone()],
        });
    }

    // ── 4. Error escalation hint ──
    let err_count = snapshot.tool_errors.len();
    if err_count >= 5 {
        hints.push(ObservationActionHint {
            target_type: "error_escalation".to_string(),
            summary: format!(
                "{err_count} recent tool errors — verify environment/tool availability; consider reporting to user"
            ),
            confidence: ObservationConfidence::classification_evidence(0.80, 0.85),
            observation_refs: vec![evidence_ref.clone()],
        });
    }

    // ── 5. Batching advice ──
    let single_tool_streak = snapshot
        .recent_rounds
        .iter()
        .rev()
        .take_while(|r| r.tool_call_names.len() == 1)
        .count();
    if single_tool_streak >= 3 {
        hints.push(ObservationActionHint {
            target_type: "batching_advice".to_string(),
            summary: format!(
                "{single_tool_streak} consecutive rounds with single tool calls — batch independent reads for efficiency"
            ),
            confidence: ObservationConfidence::classification_evidence(0.55, 0.65),
            observation_refs: vec![evidence_ref.clone()],
        });
    }

    // ── 6. Loop guard hint ──
    let recent_names: Vec<&[String]> = snapshot
        .recent_rounds
        .iter()
        .rev()
        .take(6)
        .map(|r| r.tool_call_names.as_slice())
        .collect();
    if recent_names.len() >= 3 {
        let last = recent_names[0];
        if !last.is_empty() && recent_names[1..].iter().take(2).all(|r| *r == last) {
            hints.push(ObservationActionHint {
                target_type: "loop_guard".to_string(),
                summary: format!(
                    "3+ consecutive rounds with identical tool pattern [{}] — may be stuck in exploration loop",
                    last.join(", ")
                ),
                confidence: ObservationConfidence::classification_evidence(0.50, 0.60),
                observation_refs: vec![evidence_ref],
            });
        }
    }

    hints
}

/// Build an observation graph slice from the introspect snapshot and
/// computed observations/evidence/action-hints. This mirrors the pattern
/// used in reflect's [`build_reflect_graph_slice`] but for live runtime data.
fn build_introspect_graph_slice(
    snapshot: &IntrospectSnapshot,
    request: &IntrospectRequest,
    observations: &[ObservationRecord],
    evidence: &[ObservationEvidence],
    action_hints: &[ObservationActionHint],
) -> ObservationGraphSlice {
    let mut nodes = Vec::new();
    let mut node_refs = BTreeSet::new();
    let mut edges = Vec::new();
    let mut edge_keys = BTreeSet::new();

    // ── evidence nodes (layer: Runtime, kind: Evidence) ──
    for item in evidence {
        push_graph_node(
            &mut nodes,
            &mut node_refs,
            ObservationGraphNode {
                ref_id: item.ref_id.clone(),
                layer: ObservationGraphLayer::Runtime,
                kind: ObservationGraphNodeKind::Evidence,
                label: item.evidence_class.clone(),
                summary: Some(item.summary.clone()),
                metadata: None,
            },
        );
    }

    // ── observation nodes (layer: Observation, kind: Observation) ──
    for obs in observations {
        push_graph_node(
            &mut nodes,
            &mut node_refs,
            ObservationGraphNode {
                ref_id: obs.ref_id.clone(),
                layer: ObservationGraphLayer::Observation,
                kind: ObservationGraphNodeKind::Observation,
                label: obs.kind.clone(),
                summary: Some(obs.summary.clone()),
                metadata: None,
            },
        );
        // Link observation → evidence (DerivedFrom)
        for ev_ref in &obs.evidence_refs {
            push_graph_edge(
                &mut edges,
                &mut edge_keys,
                obs.ref_id.clone(),
                ev_ref.clone(),
                ObservationGraphEdgeKind::DerivedFrom,
            );
        }
    }

    // ── action hints (layer: Observation, kind: Observation) ──
    for hint in action_hints {
        push_graph_node(
            &mut nodes,
            &mut node_refs,
            ObservationGraphNode {
                ref_id: Urn::new("observation", "local", "introspect")
                    .seg("hint")
                    .seg(&hint.target_type)
                    .build(),
                layer: ObservationGraphLayer::Observation,
                kind: ObservationGraphNodeKind::Observation,
                label: "action_hint".to_string(),
                summary: Some(hint.summary.clone()),
                metadata: None,
            },
        );
    }

    // ── tool error nodes (layer: Runtime, kind: Outcome) ──
    if matches!(
        request.facet,
        ObservationFacet::Errors | ObservationFacet::Overview | ObservationFacet::Session
    ) {
        for (idx, error) in snapshot.tool_errors.iter().enumerate() {
            let error_ref = Urn::new("observation", "local", "introspect")
                .seg("execution")
                .seg("error")
                .idx(idx)
                .build();
            let label = error
                .failure_category
                .as_deref()
                .map(|category| format!("tool_error:{category}"))
                .unwrap_or_else(|| "tool_error".to_string());
            push_graph_node(
                &mut nodes,
                &mut node_refs,
                ObservationGraphNode {
                    ref_id: error_ref.clone(),
                    layer: ObservationGraphLayer::Runtime,
                    kind: classify_event_kind("tool_error"),
                    label,
                    summary: truncate_graph_summary(
                        error
                            .error_preview
                            .as_deref()
                            .filter(|preview| !preview.trim().is_empty())
                            .unwrap_or(&error.signature_hint),
                        140,
                    ),
                    metadata: Some(serde_json::json!({
                        "tool": error.tool,
                        "failure_category": error.failure_category,
                        "file_path": error.file_path,
                        "file_range": error.file_range,
                        "turn": error.turn,
                        "round": error.round,
                    })),
                },
            );
        }
    }

    // ── tool health nodes (layer: Runtime, kind: Outcome) ──
    if matches!(
        request.facet,
        ObservationFacet::Session | ObservationFacet::Overview
    ) {
        for tool in snapshot.tool_health.iter().filter(|tool| {
            tool.avoidance_advised
                || tool.errors > 0
                || tool.input_validation_failures > 0
                || tool.consecutive_failures > 0
        }) {
            let health_ref = Urn::new("observation", "local", "introspect")
                .seg("execution")
                .seg("tool")
                .seg(&tool.name)
                .build();
            let severity = if tool.avoidance_advised || tool.consecutive_failures >= 3 {
                "warning"
            } else {
                "info"
            };
            push_graph_node(
                &mut nodes,
                &mut node_refs,
                ObservationGraphNode {
                    ref_id: health_ref.clone(),
                    layer: ObservationGraphLayer::Runtime,
                    kind: ObservationGraphNodeKind::Outcome,
                    label: "tool_health".to_string(),
                    summary: Some(format!(
                        "{name} calls={calls} errors={errors} input_validation_failures={ivf} consecutive_failures={cf}",
                        name = tool.name,
                        calls = tool.calls,
                        errors = tool.errors,
                        ivf = tool.input_validation_failures,
                        cf = tool.consecutive_failures
                    )),
                    metadata: Some(serde_json::json!({
                        "tool_name": tool.name,
                        "calls": tool.calls,
                        "errors": tool.errors,
                        "input_validation_failures": tool.input_validation_failures,
                        "consecutive_failures": tool.consecutive_failures,
                        "avoidance_advised": tool.avoidance_advised,
                        "severity": severity,
                    })),
                },
            );
        }
    }

    // ── stall nodes (layer: Runtime, kind: Event) ──
    if matches!(
        request.facet,
        ObservationFacet::Stall | ObservationFacet::Overview
    ) {
        if !snapshot.stall_state.events.is_empty() || snapshot.stall_state.introspection_count > 0 {
            push_graph_node(
                &mut nodes,
                &mut node_refs,
                ObservationGraphNode {
                    ref_id: "urn:astra:observation:local:introspect:stall:state".to_string(),
                    layer: ObservationGraphLayer::Runtime,
                    kind: ObservationGraphNodeKind::Event,
                    label: "stall_telemetry".to_string(),
                    summary: Some(format!(
                        "Stall guard: {} events, {} introspections",
                        snapshot.stall_state.events.len(),
                        snapshot.stall_state.introspection_count,
                    )),
                    metadata: Some(serde_json::json!({
                        "event_count": snapshot.stall_state.events.len(),
                        "introspection_count": snapshot.stall_state.introspection_count,
                        "advisory_signals": snapshot.stall_state.advisory_signals,
                    })),
                },
            );
        }
        for correction in &snapshot.stall_state.advisory_signals {
            push_graph_node(
                &mut nodes,
                &mut node_refs,
                ObservationGraphNode {
                    ref_id: Urn::new("observation", "local", "introspect")
                        .seg("stall")
                        .seg("correction")
                        .seg(correction)
                        .build(),
                    layer: ObservationGraphLayer::Runtime,
                    kind: ObservationGraphNodeKind::Outcome,
                    label: "stall_advisory_signal".to_string(),
                    summary: Some(format!("advisory signal emitted: {correction}")),
                    metadata: None,
                },
            );
        }
    }

    ObservationGraphSlice {
        nodes,
        edges,
        budget_result: ObservationBudgetResult::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::introspect::{
        CircuitBreakerSnapshot, IntrospectSnapshot, InvocationLifecycleSnapshot,
        RoundSnapshotEntry, StallSnapshotSummary, StepLatencySnapshotEntry, ToolErrorEntry,
        ToolHealthEntry, VolatileSnapshotEntry,
    };
    use astra_core::{ObservationDepth, ObservationFacet, ObservationTopic};

    fn tool_error_entry(tool: &str, preview: &str, category: Option<&str>) -> ToolErrorEntry {
        ToolErrorEntry {
            tool: tool.to_string(),
            signature_hint: format!("{tool} failed"),
            failure_category: category.map(String::from),
            error_preview: Some(preview.to_string()),
            at_epoch: 1000,
            error_message: String::new(),
            file_path: None,
            file_range: None,
            turn: 1,
            round: 1,
        }
    }

    fn unhealthy_tool(name: &str) -> ToolHealthEntry {
        ToolHealthEntry {
            name: name.to_string(),
            calls: 10,
            errors: 3,
            input_validation_failures: 0,
            avg_ms: 500,
            avoidance_advised: true,
            consecutive_failures: 2,
            last_failure_category: Some("timeout".to_string()),
        }
    }

    #[test]
    fn json_report_includes_step_latency_observation() {
        let snapshot = IntrospectSnapshot {
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
        let request = IntrospectRequest {
            topic: ObservationTopic::Execution,
            facet: ObservationFacet::Trace,
            ..Default::default()
        };

        let report = build_introspect_report(&snapshot, &request);
        let observation = report
            .observations
            .iter()
            .find(|observation| observation.kind == "step_latency")
            .expect("json introspect report should expose step latency");

        assert_eq!(observation.topic, "execution");
        assert_eq!(observation.facet, "trace");
        assert!(observation.summary.contains("dominant=model_wait"));
        assert!(observation.summary.contains("pre_tool_wait_ms=8000"));
        assert!(observation.summary.contains("first_tool=bash"));
        assert!(
            report
                .graph_slice
                .nodes
                .iter()
                .any(|node| node.label == "step_latency"),
            "step latency observation should also be reachable from graph_slice"
        );
    }

    #[test]
    fn overview_summary_exposes_composite_facets_before_bounded_projection() {
        let snapshot = IntrospectSnapshot {
            recent_rounds: vec![RoundSnapshotEntry {
                turn: 2,
                round: 4,
                model: "deepseek-flash".into(),
                prompt_tokens: 100,
                cache_read_tokens: 900,
                completion_tokens: 20,
                tool_call_names: vec!["introspect".into()],
                duration_ms: 1200,
                finish_reason: Some("tool_calls".into()),
                ..Default::default()
            }],
            tool_errors: vec![tool_error_entry("bash", "command not found", None)],
            step_latency: vec![StepLatencySnapshotEntry {
                step_id: "turn-2-step-4".into(),
                total_ms: Some(1200),
                pre_tool_wait_ms: Some(1100),
                tool_execution_ms: 10,
                tool_call_count: 1,
                dominant_phase: "model_wait".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let request = IntrospectRequest {
            facet: ObservationFacet::Overview,
            depth: ObservationDepth::Diagnostic,
            ..Default::default()
        };

        let report = build_introspect_report(&snapshot, &request);
        assert!(
            report
                .summary
                .contains("composite_facets=runtime,recent,errors,trace")
        );
        assert!(
            report
                .summary
                .contains("latest_round=t2_r4 model=deepseek-flash")
        );
        assert!(
            report
                .summary
                .contains("error_samples=bash:command not found")
        );
        assert!(report.summary.contains("latest_trace=step:turn-2-step-4"));
        assert!(report.summary.contains("overview_is_one_snapshot"));
    }

    #[test]
    fn evidence_revision_ignores_diagnostic_churn_but_tracks_external_observation() {
        let base = IntrospectSnapshot {
            tool_health: vec![unhealthy_tool("bash"), unhealthy_tool("introspect")],
            recent_rounds: vec![
                RoundSnapshotEntry {
                    turn: 1,
                    round: 1,
                    tool_call_names: vec!["introspect".into()],
                    ..Default::default()
                },
                RoundSnapshotEntry {
                    turn: 1,
                    round: 2,
                    tool_call_names: vec!["bash".into()],
                    ..Default::default()
                },
            ],
            tool_errors: vec![tool_error_entry("reflect", "diagnostic", None)],
            stall_state: StallSnapshotSummary {
                events: vec!["introspect observation".into(), "external stall".into()],
                advisory_signals: vec!["reflect churn".into(), "real gap".into()],
                introspection_count: 1,
            },
            invocation_lifecycle: Some(InvocationLifecycleSnapshot::default()),
            ..Default::default()
        };
        let request = IntrospectRequest {
            facet: ObservationFacet::Overview,
            ..Default::default()
        };
        let first = evidence_revision(&base, &request);

        let mut diagnostic_churn = base.clone();
        diagnostic_churn.tool_health[1].calls += 10;
        diagnostic_churn.recent_rounds.insert(
            0,
            RoundSnapshotEntry {
                turn: 1,
                round: 3,
                tool_call_names: vec!["introspect".into(), "reflect".into()],
                ..Default::default()
            },
        );
        diagnostic_churn.stall_state.introspection_count = 99;
        diagnostic_churn.invocation_lifecycle = Some(InvocationLifecycleSnapshot {
            hot_total: 99,
            prepared: 98,
            dispatched: 97,
            succeeded: 96,
            ..Default::default()
        });
        let second = evidence_revision(&diagnostic_churn, &request);
        assert_eq!(
            first, second,
            "diagnostic self-observation is not task evidence"
        );

        diagnostic_churn.recent_rounds[1].tool_call_names = vec!["bash".into(), "read_file".into()];
        let third = evidence_revision(&diagnostic_churn, &request);
        assert_ne!(
            second, third,
            "a changed external observation advances evidence"
        );

        let mut diagnostic_failure = diagnostic_churn.clone();
        diagnostic_failure.tool_errors.push(tool_error_entry(
            "reflect",
            "new diagnostic failure",
            None,
        ));
        assert_ne!(
            second,
            evidence_revision(&diagnostic_failure, &request),
            "a new observation failure is a diagnostic fact, not acquisition churn"
        );
    }

    #[test]
    fn evidence_revision_tracks_runtime_diagnostic_state_without_observer_counters() {
        let request = IntrospectRequest::default();
        let base = IntrospectSnapshot {
            semantic_judgments: Some(
                astra_services::semantic_judgment_observation::SemanticJudgmentView {
                    counts: Some(Default::default()),
                    ..Default::default()
                },
            ),
            ..Default::default()
        };
        let first = evidence_revision(&base, &request);

        let mut semantic = base.clone();
        semantic.semantic_judgments = Some(
            astra_services::semantic_judgment_observation::SemanticJudgmentView {
                counts: Some(
                    astra_services::semantic_judgment_observation::SemanticJudgmentStageCounts {
                        evaluated: 1,
                        decisions: 1,
                        ..Default::default()
                    },
                ),
                ..Default::default()
            },
        );
        assert_eq!(
            first,
            evidence_revision(&semantic, &request),
            "ordinary successful judgment counts do not make the primary runtime snapshot new"
        );
        let forensic_request = IntrospectRequest {
            depth: astra_core::ObservationDepth::Forensic,
            ..request.clone()
        };
        assert_ne!(
            evidence_revision(&base, &forensic_request),
            evidence_revision(&semantic, &forensic_request),
            "forensic judgment reads opt into successful classification detail"
        );

        let mut usage = base.clone();
        usage.judgment_usage = Some(crate::introspect::JudgmentUsageSnapshot {
            coverage: crate::introspect::JudgmentUsageCoverage::Available,
            input_complete: true,
            output_complete: true,
            observed_attempts: Some(1),
            known_input_tokens: Some(100),
            known_output_tokens: Some(10),
            ..Default::default()
        });
        let usage_first = evidence_revision(&usage, &request);
        let mut usage_more = usage.clone();
        usage_more
            .judgment_usage
            .as_mut()
            .unwrap()
            .observed_attempts = Some(2);
        usage_more
            .judgment_usage
            .as_mut()
            .unwrap()
            .known_input_tokens = Some(200);
        assert_eq!(
            usage_first,
            evidence_revision(&usage_more, &request),
            "normal successful judgment usage growth is auxiliary telemetry"
        );
        usage_more.judgment_usage.as_mut().unwrap().input_complete = false;
        assert_ne!(
            usage_first,
            evidence_revision(&usage_more, &request),
            "judgment usage completeness loss is diagnostic evidence"
        );

        let mut selection = base.clone();
        selection.tool_result_judgments = Some(
            astra_services::tool_result_selection_observation::ToolResultJudgmentView {
                evaluation_coverage:
                    astra_services::tool_result_selection_observation::ToolResultJudgmentCoverage::Available,
                application_coverage:
                    astra_services::tool_result_selection_observation::ToolResultJudgmentCoverage::Available,
                selected: 1,
                applications:
                    astra_services::tool_result_selection_observation::ToolResultApplicationCounts {
                        included: 1,
                        ..Default::default()
                    },
                ..Default::default()
            },
        );
        let selection_first = evidence_revision(&selection, &request);
        let mut selection_more = selection.clone();
        selection_more
            .tool_result_judgments
            .as_mut()
            .unwrap()
            .selected = 2;
        selection_more
            .tool_result_judgments
            .as_mut()
            .unwrap()
            .applications
            .included = 2;
        assert_eq!(
            selection_first,
            evidence_revision(&selection_more, &request),
            "normal successful selection growth is auxiliary telemetry"
        );
        selection_more
            .tool_result_judgments
            .as_mut()
            .unwrap()
            .conflicting = 1;
        assert_ne!(
            selection_first,
            evidence_revision(&selection_more, &request),
            "a conflicting selection is diagnostic evidence"
        );

        let mut volatile = base.clone();
        volatile.volatile_pending.push(VolatileSnapshotEntry {
            kind: "ActiveWorkSnapshot".into(),
            content: "new work fact".into(),
            round_index: 4,
        });
        assert_ne!(first, evidence_revision(&volatile, &request));

        let mut stall = base.clone();
        stall.stall_state.events.push("sig_stall @ turn 4".into());
        assert_ne!(first, evidence_revision(&stall, &request));

        let mut breaker = base.clone();
        breaker.circuit_breaker = Some(CircuitBreakerSnapshot {
            state: "open".into(),
            failure_count: 99,
            success_count: 0,
            consecutive_failures: 99,
        });
        assert_ne!(first, evidence_revision(&breaker, &request));

        let mut lifecycle = base.clone();
        lifecycle.invocation_lifecycle = Some(InvocationLifecycleSnapshot {
            dispatched: 1,
            outcome_unknown: 1,
            ..Default::default()
        });
        assert_ne!(first, evidence_revision(&lifecycle, &request));

        let mut freshness = base;
        freshness.injection_freshness = vec![crate::injection_tracking::ChannelFreshness {
            channel: crate::injection_tracking::InjectionChannel::Lessons,
            status: crate::injection_tracking::ChannelStatus::Fresh { rounds_alive: 1 },
            preview: "new lesson".into(),
            first_seen_round: Some(1),
        }];
        assert_ne!(first, evidence_revision(&freshness, &request));

        let mut stale = freshness.clone();
        stale.injection_freshness[0].status =
            crate::injection_tracking::ChannelStatus::Stale { rounds_alive: 12 };
        assert_ne!(
            evidence_revision(&freshness, &request),
            evidence_revision(&stale, &request),
            "fresh-to-stale is a meaningful freshness transition"
        );
    }

    #[test]
    fn validation_only_tool_misuse_is_observable_without_unhealthy_severity() {
        let snapshot = IntrospectSnapshot {
            tool_health: vec![ToolHealthEntry {
                name: "agent_fanout".to_string(),
                calls: 0,
                errors: 0,
                input_validation_failures: 3,
                avg_ms: 0,
                avoidance_advised: false,
                consecutive_failures: 0,
                last_failure_category: None,
            }],
            ..Default::default()
        };
        let request = IntrospectRequest {
            topic: ObservationTopic::Execution,
            facet: ObservationFacet::Session,
            ..Default::default()
        };

        let report = build_introspect_report(&snapshot, &request);
        let observation = report
            .observations
            .iter()
            .find(|observation| observation.kind == "tool_health")
            .expect("validation rejects must remain observable");
        assert_eq!(observation.severity, "info");
        assert!(observation.summary.contains("calls=0 errors=0"));
        assert!(observation.summary.contains("input_validation_failures=3"));
    }

    #[test]
    fn json_report_includes_capacity_provider_observations() {
        let snapshot = IntrospectSnapshot {
            capacity_provider_coverage: vec![
                crate::introspect::CapacityProviderCoverageEntry::ready(
                    astra_runtime_env::CapacityProviderType::ServerService,
                    "server-builtin",
                    vec!["web_fetch".into(), "memory".into()],
                ),
                crate::introspect::CapacityProviderCoverageEntry::unavailable(
                    astra_runtime_env::CapacityProviderType::Sandbox,
                    "workspace-executor",
                    astra_runtime_env::CapacityProviderStatus::Unbound,
                    "no_workspace_provider_bound",
                ),
            ],
            ..Default::default()
        };

        let report = build_introspect_report(&snapshot, &IntrospectRequest::default());
        let provider_observations: Vec<_> = report
            .observations
            .iter()
            .filter(|observation| observation.kind == "capacity_provider")
            .collect();

        assert_eq!(provider_observations.len(), 2);
        assert!(provider_observations.iter().any(|observation| {
            observation.summary.contains("server_service:ready")
                && observation.summary.contains("provider_id=server-builtin")
        }));
        assert!(provider_observations.iter().any(|observation| {
            observation.summary.contains("sandbox:unbound")
                && observation.summary.contains("workspace executor not bound")
                && !observation.summary.contains("no_workspace_provider_bound")
        }));
    }

    #[test]
    fn report_evidence_preserves_runtime_progress_and_snapshot_age() {
        let snapshot = IntrospectSnapshot {
            runtime_feedback: Some(crate::introspect::test_runtime_feedback(3, 3, 0)),
            snapshot_age_turns: 2,
            ..Default::default()
        };

        let report = build_introspect_report(&snapshot, &IntrospectRequest::default());
        assert!(
            report.summary.contains("remaining=0"),
            "summary should preserve exhausted slice state: {}",
            report.summary
        );
        let evidence_summary = &report.evidence[0].summary;
        assert!(
            evidence_summary.contains("remaining=0"),
            "evidence should preserve unlimited turn budget: {evidence_summary}"
        );
        assert!(
            evidence_summary.contains("snapshot_age_turns=2"),
            "evidence should expose staleness: {evidence_summary}"
        );
    }

    // ── graph_slice tests ──

    #[test]
    fn graph_slice_empty_snapshot_has_only_evidence_node() {
        let snapshot = IntrospectSnapshot::default();
        let request = IntrospectRequest::default();
        let observations = Vec::new();
        let evidence = vec![ObservationEvidence {
            ref_id: "urn:astra:context:local:introspect:runtime_snapshot".into(),
            evidence_class: "observed_evidence".into(),
            source: "runtime.introspect_snapshot".into(),
            summary: "pressure=0% prompt_cache_read_share=0% prompt_cache_scope=current_runtime_snapshot input_total=0 cached_read=0 cache_create=0 turns=0/∞ signals=0 tool_failures=0".into(),
            confidence: ObservationConfidence::evidence(0.75),
        }];
        let action_hints = Vec::new();

        let slice = build_introspect_graph_slice(
            &snapshot,
            &request,
            &observations,
            &evidence,
            &action_hints,
        );
        // Empty snapshot with Session facet should only have the evidence node.
        assert_eq!(slice.nodes.len(), 1);
        assert_eq!(slice.nodes[0].kind, ObservationGraphNodeKind::Evidence);
        assert!(slice.edges.is_empty());
    }

    #[test]
    fn graph_slice_tool_errors_facet_includes_error_nodes() {
        let snapshot = IntrospectSnapshot {
            tool_errors: vec![tool_error_entry(
                "bash",
                "timeout after 30s",
                Some("timeout"),
            )],
            ..Default::default()
        };
        let request = IntrospectRequest {
            topic: ObservationTopic::Execution,
            facet: ObservationFacet::Errors,
            ..Default::default()
        };
        let observations = vec![ObservationRecord {
            ref_id: "urn:astra:observation:local:introspect:errors:recent".into(),
            topic: "execution".into(),
            facet: "errors".into(),
            kind: "tool_failure_cluster".into(),
            severity: "warning".into(),
            summary: "1 recent tool errors recorded".into(),
            confidence: ObservationConfidence::evidence(0.85),
            evidence_refs: vec!["urn:astra:context:local:introspect:runtime_snapshot".into()],
        }];
        let evidence = vec![ObservationEvidence {
            ref_id: "urn:astra:context:local:introspect:runtime_snapshot".into(),
            evidence_class: "observed_evidence".into(),
            source: "runtime.introspect_snapshot".into(),
            summary: "pressure=0% prompt_cache_read_share=0% prompt_cache_scope=current_runtime_snapshot input_total=0 cached_read=0 cache_create=0 turns=0/∞ signals=0 tool_failures=1".into(),
            confidence: ObservationConfidence::evidence(0.75),
        }];
        let action_hints = Vec::new();

        let slice = build_introspect_graph_slice(
            &snapshot,
            &request,
            &observations,
            &evidence,
            &action_hints,
        );

        // evidence + observation + 1 error node = 3
        assert_eq!(slice.nodes.len(), 3, "expected 3 nodes, got: {slice:#?}");
        let kinds: Vec<_> = slice.nodes.iter().map(|n| n.kind).collect();
        assert!(kinds.contains(&ObservationGraphNodeKind::Evidence));
        assert!(kinds.contains(&ObservationGraphNodeKind::Observation));
        assert!(kinds.contains(&ObservationGraphNodeKind::Outcome));
        // observation → evidence (DerivedFrom)
        assert_eq!(slice.edges.len(), 1);
        assert_eq!(slice.edges[0].kind, ObservationGraphEdgeKind::DerivedFrom);
    }

    #[test]
    fn graph_slice_session_facet_includes_tool_health_nodes() {
        let snapshot = IntrospectSnapshot {
            tool_health: vec![unhealthy_tool("read_file")],
            ..Default::default()
        };
        let request = IntrospectRequest {
            topic: ObservationTopic::Runtime,
            facet: ObservationFacet::Session,
            ..Default::default()
        };
        let observations = Vec::new();
        let evidence = vec![ObservationEvidence {
            ref_id: "urn:astra:context:local:introspect:runtime_snapshot".into(),
            evidence_class: "observed_evidence".into(),
            source: "runtime.introspect_snapshot".into(),
            summary: "healthy runtime".into(),
            confidence: ObservationConfidence::evidence(0.75),
        }];
        let action_hints = Vec::new();

        let slice = build_introspect_graph_slice(
            &snapshot,
            &request,
            &observations,
            &evidence,
            &action_hints,
        );

        // evidence + tool_health = 2 nodes
        assert_eq!(slice.nodes.len(), 2);
        let health_node = slice
            .nodes
            .iter()
            .find(|n| n.kind == ObservationGraphNodeKind::Outcome && n.label == "tool_health")
            .expect("should have tool health node");
        assert_eq!(health_node.layer, ObservationGraphLayer::Runtime);
        assert_eq!(slice.edges.len(), 0);
    }

    #[test]
    fn graph_slice_errors_facet_excludes_tool_health_nodes() {
        let snapshot = IntrospectSnapshot {
            tool_errors: vec![tool_error_entry("bash", "command not found", None)],
            tool_health: vec![unhealthy_tool("bash")],
            ..Default::default()
        };
        let request = IntrospectRequest {
            topic: ObservationTopic::Execution,
            facet: ObservationFacet::Errors,
            ..Default::default()
        };
        let observations = Vec::new();
        let evidence = vec![ObservationEvidence {
            ref_id: "urn:astra:context:local:introspect:runtime_snapshot".into(),
            evidence_class: "observed_evidence".into(),
            source: "runtime.introspect_snapshot".into(),
            summary: "errors present".into(),
            confidence: ObservationConfidence::evidence(0.75),
        }];
        let action_hints = Vec::new();

        let slice = build_introspect_graph_slice(
            &snapshot,
            &request,
            &observations,
            &evidence,
            &action_hints,
        );

        // evidence + 1 error node, no tool_health node
        assert_eq!(slice.nodes.len(), 2);
        let labels: Vec<_> = slice.nodes.iter().map(|n| n.label.as_str()).collect();
        assert!(labels.contains(&"tool_error:timeout") || labels.contains(&"tool_error"));
        // No tool_health node in errors facet
        assert!(!slice.nodes.iter().any(|n| n.label == "tool_health"));
    }

    #[test]
    fn graph_slice_dedups_duplicate_ref_ids() {
        let snapshot = IntrospectSnapshot {
            tool_errors: vec![tool_error_entry(
                "bash",
                "timeout after 30s",
                Some("timeout"),
            )],
            ..Default::default()
        };
        let request = IntrospectRequest {
            topic: ObservationTopic::Execution,
            facet: ObservationFacet::Errors,
            ..Default::default()
        };
        // Observation ref_id overlaps with the error node ref — dedup should prevent
        // adding the same ref_id twice.
        let observations = vec![ObservationRecord {
            ref_id: "urn:astra:observation:local:introspect:execution:error:0".into(),
            topic: "execution".into(),
            facet: "errors".into(),
            kind: "tool_error".into(),
            severity: "warning".into(),
            summary: "bash timeout after 30s".into(),
            confidence: ObservationConfidence::evidence(0.80),
            evidence_refs: vec!["urn:astra:context:local:introspect:runtime_snapshot".into()],
        }];
        let evidence = vec![ObservationEvidence {
            ref_id: "urn:astra:context:local:introspect:runtime_snapshot".into(),
            evidence_class: "observed_evidence".into(),
            source: "runtime.introspect_snapshot".into(),
            summary: "errors present".into(),
            confidence: ObservationConfidence::evidence(0.75),
        }];
        let action_hints = Vec::new();

        let slice = build_introspect_graph_slice(
            &snapshot,
            &request,
            &observations,
            &evidence,
            &action_hints,
        );

        // The observation node is added first (observations step), then the
        // tool_error node is skipped because its ref_id matches. So evidence (1)
        // + observation (1) = 2. The node keeps Observation kind (first-writer wins).
        assert_eq!(slice.nodes.len(), 2);
        let error_node = slice
            .nodes
            .iter()
            .find(|n| n.ref_id.contains("error:0"))
            .expect("should have error node");
        assert_eq!(error_node.kind, ObservationGraphNodeKind::Observation);
    }

    #[test]
    fn stall_facet_preserves_facts_without_derived_strategy_pressure() {
        let snapshot = IntrospectSnapshot {
            stall_state: StallSnapshotSummary {
                events: vec!["sig_stall @ turn 5".into()],
                introspection_count: 2,
                advisory_signals: vec!["parallel_batching".into()],
            },
            ..Default::default()
        };
        let request = IntrospectRequest {
            topic: ObservationTopic::Execution,
            facet: ObservationFacet::Stall,
            ..Default::default()
        };
        let observations = Vec::new();
        let evidence = vec![ObservationEvidence {
            ref_id: "urn:astra:context:local:introspect:runtime_snapshot".into(),
            evidence_class: "observed_evidence".into(),
            source: "runtime.introspect_snapshot".into(),
            summary: "stall detected".into(),
            confidence: ObservationConfidence::evidence(0.75),
        }];
        let action_hints = Vec::new();

        let slice = build_introspect_graph_slice(
            &snapshot,
            &request,
            &observations,
            &evidence,
            &action_hints,
        );

        // Evidence + observed stall state + advisory label = 3
        assert_eq!(slice.nodes.len(), 3);
        let labels: Vec<_> = slice.nodes.iter().map(|n| n.label.as_str()).collect();
        assert!(labels.contains(&"stall_telemetry"));
        assert!(labels.contains(&"stall_advisory_signal"));
        let metadata = slice
            .nodes
            .iter()
            .find(|node| node.label == "stall_telemetry")
            .unwrap()
            .metadata
            .as_ref()
            .unwrap();
        assert_eq!(metadata["event_count"], 1);
        assert_eq!(metadata["introspection_count"], 2);
        assert!(metadata.get("nudge_count").is_none());

        let observations = build_introspect_observations(
            &snapshot,
            &request,
            &introspect_summary(&snapshot, &request),
        );
        assert!(observations.iter().any(|obs| obs.kind == "stall_telemetry"));
        assert!(
            observations
                .iter()
                .any(|obs| obs.kind == "stall_advisory_signal")
        );
        assert!(build_introspect_action_hints(&snapshot, &observations).is_empty());

        // Introspection-only state is still observable; it is not nudge pressure.
        let mut introspection_only = snapshot;
        introspection_only.stall_state.events.clear();
        introspection_only.stall_state.advisory_signals.clear();
        let observations = build_introspect_observations(
            &introspection_only,
            &request,
            &introspect_summary(&introspection_only, &request),
        );
        assert!(observations.iter().any(|obs| obs.kind == "stall_telemetry"));
        assert!(build_introspect_action_hints(&introspection_only, &observations).is_empty());
    }

    #[test]
    fn graph_slice_budget_result_is_default() {
        let slice = build_introspect_graph_slice(
            &IntrospectSnapshot::default(),
            &IntrospectRequest::default(),
            &[],
            &[],
            &[],
        );
        // Graph budget_result is always default — budget is applied at the report
        // level, not the graph level.
        assert_eq!(slice.budget_result, ObservationBudgetResult::default());
    }
}
