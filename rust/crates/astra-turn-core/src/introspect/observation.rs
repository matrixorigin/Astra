use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use astra_core::{
    ObservationActionHint, ObservationAdaptationSignal, ObservationBudgetOmitted,
    ObservationBudgetResult, ObservationCausalChain, ObservationConfidence,
    ObservationDataCoverage, ObservationEvidence, ObservationFailureCluster, ObservationGraphSlice,
    ObservationProviderCoverage, ObservationRecord, ObservationView,
};

use super::{IntrospectRequest, IntrospectSnapshot, ObservationFacet, SourcePolicy};

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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observations: Vec<ObservationRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<ObservationEvidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub action_hints: Vec<ObservationActionHint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_clusters: Vec<ObservationFailureCluster>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causal_chains: Vec<ObservationCausalChain>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adaptation_signals: Vec<ObservationAdaptationSignal>,
    #[serde(default)]
    pub graph_slice: ObservationGraphSlice,
    #[serde(default)]
    pub budget_result: ObservationBudgetResult,
}

pub fn build_introspect_report(
    snapshot: &IntrospectSnapshot,
    request: &IntrospectRequest,
) -> IntrospectReport {
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
            "pressure={:.0}% cache={:.0}% turns={}/{} alerts={} tool_errors={}",
            snapshot.token_pressure * 100.0,
            snapshot.cache_hit_ratio * 100.0,
            snapshot.turns_completed,
            snapshot.turns_completed + snapshot.turns_remaining,
            snapshot.alerts.len(),
            snapshot.tool_errors.len(),
        ),
        confidence: ObservationConfidence::evidence(0.75),
    }];

    let mut action_hints = build_introspect_action_hints(snapshot, &observations);
    let budget_result =
        apply_report_budget(request, &mut observations, &mut evidence, &mut action_hints);

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
        causal_chains: Vec::new(),
        adaptation_signals: Vec::new(),
        graph_slice: ObservationGraphSlice::default(),
        budget_result,
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
    let omitted_observations = truncate_count(observations, max_observations);
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
    let omitted_evidence = truncate_count(evidence, max_evidence);
    let omitted_hints = truncate_count(action_hints, max_hints);
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

fn truncate_count<T>(items: &mut Vec<T>, max: usize) -> i64 {
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
            "Runtime healthy - pressure {:.0}%, cache {:.0}%, turns {}/{}",
            snapshot.token_pressure * 100.0,
            snapshot.cache_hit_ratio * 100.0,
            snapshot.turns_completed,
            snapshot.turns_completed + snapshot.turns_remaining
        ),
    }
}

fn build_introspect_observations(
    snapshot: &IntrospectSnapshot,
    request: &IntrospectRequest,
    summary: &str,
) -> Vec<ObservationRecord> {
    let mut observations = Vec::new();
    match request.facet {
        ObservationFacet::Session => {
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
            } else {
                if !snapshot.alerts.is_empty() {
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
        }
        ObservationFacet::Stall => {
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
        }
        _ => {}
    }

    for (idx, error) in snapshot.tool_errors.iter().enumerate() {
        observations.push(ObservationRecord {
            ref_id: format!("urn:astra:observation:local:introspect:execution:error:{idx}"),
            topic: "execution".to_string(),
            facet: "errors".to_string(),
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

    for tool in snapshot
        .tool_health
        .iter()
        .filter(|tool| tool.avoidance_advised || tool.errors > 0 || tool.consecutive_failures > 0)
        .take(8)
    {
        observations.push(ObservationRecord {
            ref_id: format!(
                "urn:astra:observation:local:introspect:execution:tool:{}",
                urn_component(&tool.name)
            ),
            topic: "execution".to_string(),
            facet: "tools".to_string(),
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

    observations
}

fn build_introspect_action_hints(
    snapshot: &IntrospectSnapshot,
    observations: &[ObservationRecord],
) -> Vec<ObservationActionHint> {
    snapshot
        .tool_health
        .iter()
        .filter(|tool| tool.avoidance_advised)
        .take(5)
        .filter_map(|tool| {
            let tool_ref = format!(
                "urn:astra:observation:local:introspect:execution:tool:{}",
                urn_component(&tool.name)
            );
            let observation_refs: Vec<String> = observations
                .iter()
                .filter(|observation| observation.ref_id == tool_ref)
                .map(|observation| observation.ref_id.clone())
                .collect();

            if observation_refs.is_empty() {
                return None;
            }

            Some(ObservationActionHint {
                target_type: "tool_policy".to_string(),
                summary: format!("Avoid or verify {} until health recovers", tool.name),
                confidence: ObservationConfidence::classification_evidence(0.70, 0.75),
                observation_refs,
            })
        })
        .collect()
}

fn urn_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len().max(1));
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}
