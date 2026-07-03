use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use astra_core::{
    ObservationActionHint, ObservationBudgetOmitted, ObservationBudgetResult,
    ObservationConfidence, ObservationDataCoverage, ObservationEvidence, ObservationFailureCluster,
    ObservationGraphEdgeKind, ObservationGraphLayer, ObservationGraphNode,
    ObservationGraphNodeKind, ObservationGraphSlice, ObservationProviderCoverage,
    ObservationRecord, ObservationView, SourcePolicy, Urn, classify_event_kind, push_graph_edge,
    push_graph_node, truncate_graph_summary,
};

use super::{IntrospectRequest, IntrospectSnapshot, ObservationFacet, turn_budget_label};

const RUNTIME_SNAPSHOT_REF: &str = "urn:astra:context:local:introspect:runtime_snapshot";

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
    pub data_coverage: ObservationDataCoverage,
    pub summary: String,
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
    let data_coverage = introspect_data_coverage(snapshot, request, warnings);
    let view = ObservationView {
        topic: request.topic.as_str().to_string(),
        facet: request.facet.as_str().to_string(),
        depth: request.depth.as_str().to_string(),
        horizon: request.horizon.as_str().to_string(),
        data_coverage: data_coverage.clone(),
    };

    let summary = introspect_summary(snapshot, request);
    let mut observations = build_introspect_observations(snapshot, request, &summary);

    let mut evidence = vec![ObservationEvidence {
        ref_id: RUNTIME_SNAPSHOT_REF.to_string(),
        evidence_class: "observed_evidence".to_string(),
        source: "runtime.introspect_snapshot".to_string(),
        summary: format!(
            "pressure={:.0}% cache={:.0}% turns={} signals={} tool_failures={}{}",
            snapshot.token_pressure * 100.0,
            snapshot.cache_hit_ratio * 100.0,
            turn_budget_label(snapshot),
            snapshot.alerts.len(),
            snapshot.tool_errors.len(),
            snapshot_age_suffix(snapshot),
        ),
        confidence: ObservationConfidence::evidence(0.75),
    }];

    let mut action_hints = build_introspect_action_hints(snapshot, &observations);
    let budget_result =
        apply_report_budget(request, &mut observations, &mut evidence, &mut action_hints);

    let graph_slice =
        build_introspect_graph_slice(snapshot, request, &observations, &evidence, &action_hints);

    IntrospectReport {
        schema_version: 1,
        tool: "introspect".to_string(),
        topic: request.topic.as_str().to_string(),
        facet: request.facet.as_str().to_string(),
        depth: request.depth.as_str().to_string(),
        horizon: request.horizon.as_str().to_string(),
        source_policy: request.source_policy.as_str().to_string(),
        include_context: request.include_context,
        data_coverage,
        summary,
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
        schema_version: 1,
        tool: "introspect".to_string(),
        topic: request.topic.as_str().to_string(),
        facet: facet.to_string(),
        depth: request.depth.as_str().to_string(),
        horizon: request.horizon.as_str().to_string(),
        source_policy: request.source_policy.as_str().to_string(),
        include_context: request.include_context,
        data_coverage,
        summary,
        view,
        observations,
        evidence: Vec::new(),
        action_hints: Vec::new(),
        failure_clusters: Vec::new(),
        graph_slice: ObservationGraphSlice::default(),
        budget_result: ObservationBudgetResult::default(),
    }
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
        .saturating_add(snapshot.alerts.len()) as i64
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
        decisions: 0,
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
    let (max_observations, max_evidence, max_hints) = match request.depth.as_str() {
        "hint" => (3, 1, 2),
        "summary" => (8, 4, 4),
        "diagnostic" => (32, 16, 8),
        _ => (100, 50, 8),
    };

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
fn observation_priority_key(o: &ObservationRecord) -> i64 {
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
    match request.facet {
        ObservationFacet::Errors => {
            if snapshot.tool_errors.is_empty() {
                "No recent tool errors recorded".to_string()
            } else {
                format!("{} recent tool errors recorded", snapshot.tool_errors.len())
            }
        }
        ObservationFacet::Stall => {
            if snapshot.stall_state.nudge_count == 0 && snapshot.stall_state.events.is_empty() {
                "No stall or loop-guard events recorded".to_string()
            } else {
                format!(
                    "Stall guard has {} nudges and {} events",
                    snapshot.stall_state.nudge_count,
                    snapshot.stall_state.events.len()
                )
            }
        }
        _ if !snapshot.alerts.is_empty() => {
            format!(
                "Runtime snapshot has {} active alerts",
                snapshot.alerts.len()
            )
        }
        _ => format!(
            "Runtime healthy - pressure {:.0}%, cache {:.0}%, turns {}",
            snapshot.token_pressure * 100.0,
            snapshot.cache_hit_ratio * 100.0,
            turn_budget_label(snapshot),
        ),
    }
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
                    tool.avoidance_advised || tool.errors > 0 || tool.consecutive_failures > 0
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
                        "{} calls={} errors={} consecutive_failures={}",
                        tool.name, tool.calls, tool.errors, tool.consecutive_failures
                    ),
                    confidence: ObservationConfidence::complete(0.70, 0.75, 0.35),
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

        ObservationFacet::Stall
            if snapshot.stall_state.nudge_count > 0
                || !snapshot.stall_state.forced_corrections.is_empty() =>
        {
            if snapshot.stall_state.nudge_count > 0 {
                observations.push(ObservationRecord {
                    ref_id: "urn:astra:observation:local:introspect:stall:state".to_string(),
                    topic: request.topic.as_str().to_string(),
                    facet: request.facet.as_str().to_string(),
                    kind: "stall_telemetry".to_string(),
                    severity: "info".to_string(),
                    summary: format!("stall nudge count: {}", snapshot.stall_state.nudge_count),
                    confidence: ObservationConfidence::evidence(0.90),
                    evidence_refs: vec![RUNTIME_SNAPSHOT_REF.to_string()],
                });
            }
            for correction in &snapshot.stall_state.forced_corrections {
                observations.push(ObservationRecord {
                    ref_id: Urn::new("observation", "local", "introspect")
                        .seg("stall")
                        .seg("correction")
                        .seg(correction)
                        .build(),
                    topic: request.topic.as_str().to_string(),
                    facet: request.facet.as_str().to_string(),
                    kind: "stall_forced_correction".to_string(),
                    severity: "warning".to_string(),
                    summary: format!("forced correction fired: {correction}"),
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

    // ── 2. Strategy change hint ──
    let stall = &snapshot.stall_state;
    let has_active_corrections = !stall.forced_corrections.is_empty();
    let nudge_pressure = stall.nudge_count >= 2 || stall.drift_nudge_count >= 2;
    if has_active_corrections || nudge_pressure {
        let mut parts: Vec<String> = Vec::new();
        if has_active_corrections {
            parts.push(format!(
                "{} forced correction(s): {}",
                stall.forced_corrections.len(),
                stall.forced_corrections.join(", ")
            ));
        }
        if nudge_pressure {
            parts.push(format!(
                "nudges={} drift={}",
                stall.nudge_count, stall.drift_nudge_count
            ));
        }
        hints.push(ObservationActionHint {
            target_type: "strategy_change".to_string(),
            summary: format!(
                "Multiple stall interventions — {}; consider strategy change or user clarification",
                parts.join("; ")
            ),
            confidence: ObservationConfidence::classification_evidence(0.65, 0.80),
            observation_refs: vec![evidence_ref.clone()],
        });
    }

    // ── 3. Token pressure hint ──
    if snapshot.token_pressure > 0.80 {
        hints.push(ObservationActionHint {
            target_type: "pressure_mitigation".to_string(),
            summary: format!(
                "Token pressure {:.0}% — prefer targeted reads (line ranges) over full files; batch independent calls",
                snapshot.token_pressure * 100.0
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
            tool.avoidance_advised || tool.errors > 0 || tool.consecutive_failures > 0
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
                        "{name} calls={calls} errors={errors} consecutive_failures={cf}",
                        name = tool.name,
                        calls = tool.calls,
                        errors = tool.errors,
                        cf = tool.consecutive_failures
                    )),
                    metadata: Some(serde_json::json!({
                        "tool_name": tool.name,
                        "calls": tool.calls,
                        "errors": tool.errors,
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
        if snapshot.stall_state.nudge_count > 0 {
            push_graph_node(
                &mut nodes,
                &mut node_refs,
                ObservationGraphNode {
                    ref_id: "urn:astra:observation:local:introspect:stall:state".to_string(),
                    layer: ObservationGraphLayer::Runtime,
                    kind: ObservationGraphNodeKind::Event,
                    label: "stall_telemetry".to_string(),
                    summary: Some(format!(
                        "stall nudge count: {}",
                        snapshot.stall_state.nudge_count
                    )),
                    metadata: Some(serde_json::json!({
                        "nudge_count": snapshot.stall_state.nudge_count,
                        "event_count": snapshot.stall_state.events.len(),
                        "introspection_count": snapshot.stall_state.introspection_count,
                        "forced_corrections": snapshot.stall_state.forced_corrections,
                    })),
                },
            );
        }
        for correction in &snapshot.stall_state.forced_corrections {
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
                    label: "stall_forced_correction".to_string(),
                    summary: Some(format!("forced correction fired: {correction}")),
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
        IntrospectSnapshot, StallSnapshotSummary, StepLatencySnapshotEntry, ToolErrorEntry,
        ToolHealthEntry,
    };
    use astra_core::{ObservationFacet, ObservationTopic};

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
    fn report_evidence_preserves_unlimited_turn_budget_and_snapshot_age() {
        let snapshot = IntrospectSnapshot {
            turns_completed: 3,
            turns_remaining: 0,
            turn_budget_unlimited: true,
            snapshot_age_turns: 2,
            ..Default::default()
        };

        let report = build_introspect_report(&snapshot, &IntrospectRequest::default());
        assert!(
            report.summary.contains("turns 3/∞"),
            "summary should not render 3/0 or 3/3: {}",
            report.summary
        );
        let evidence_summary = &report.evidence[0].summary;
        assert!(
            evidence_summary.contains("turns=3/∞"),
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
            summary: "pressure=0% cache=0% turns=0/∞ signals=0 tool_failures=0".into(),
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
            summary: "pressure=0% cache=0% turns=0/∞ signals=0 tool_failures=1".into(),
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
    fn graph_slice_stall_facet_includes_correction_nodes() {
        let snapshot = IntrospectSnapshot {
            stall_state: StallSnapshotSummary {
                nudge_count: 3,
                forced_corrections: vec!["execution_escalation".into()],
                ..Default::default()
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

        // evidence + stall state + correction = 3
        assert_eq!(slice.nodes.len(), 3);
        let labels: Vec<_> = slice.nodes.iter().map(|n| n.label.as_str()).collect();
        assert!(labels.contains(&"stall_telemetry"));
        assert!(labels.contains(&"stall_forced_correction"));
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
