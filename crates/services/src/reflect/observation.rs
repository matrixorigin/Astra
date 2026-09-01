use super::{
    Diagnosis, Insight, InsightKind, ObservationActionHint, ObservationConfidence,
    ObservationEvidence, ObservationFailureCluster, ObservationRecord, ReflectRecommendation,
    ReflectRecommendationSource, ReflectRequest, SessionOverview,
};
use astra_core::{ObservationGraphSlice, Urn, urn_component};

// ── Confidence constants ──────────────────────────────────────────────────────
// Named constants for observation confidence values, replacing magic numbers.
// Sources: empirical tuning from production error analysis.

/// Confidence when diagnosis has 3+ occurrences (strong signal).
const DIAGNOSIS_HIGH_CONFIDENCE: f64 = 0.90;
/// Confidence when diagnosis has samples but fewer than 3 occurrences.
const DIAGNOSIS_MEDIUM_CONFIDENCE: f64 = 0.78;
/// Confidence when diagnosis has zero evidence samples.
const DIAGNOSIS_LOW_CONFIDENCE: f64 = 0.45;

/// Causal confidence for critical-severity diagnoses.
const CAUSAL_CRITICAL_CONFIDENCE: f64 = 0.82;
/// Causal confidence for warning-severity diagnoses.
const CAUSAL_WARNING_CONFIDENCE: f64 = 0.70;
/// Causal confidence for info/low-severity diagnoses.
const CAUSAL_DEFAULT_CONFIDENCE: f64 = 0.55;

/// Base completeness confidence for a diagnosis observation.
const DIAGNOSIS_COMPLETENESS: f64 = 0.90;
/// Classification confidence for a failure cluster derived from a diagnosis.
const FAILURE_CLUSTER_CLASSIFICATION: f64 = 0.84;

/// Evidence confidence for insight-derived artifacts.
const INSIGHT_EVIDENCE_CONFIDENCE: f64 = 0.70;
/// Completeness for insight observations (with evidence).
const INSIGHT_COMPLETENESS_WITH_EV: f64 = 0.80;
/// Completeness for insight observations (without evidence).
const INSIGHT_COMPLETENESS_NO_EV: f64 = 0.45;
/// Causal confidence for insight observations.
const INSIGHT_CAUSAL: f64 = 0.40;

/// Default completeness for session health observation.
const SESSION_HEALTH_COMPLETENESS: f64 = 0.70;
/// Completeness contributor when session has events.
const SESSION_HEALTH_EVENTS_PRESENT: f64 = 0.65;
/// Completeness contributor when session has no events.
const SESSION_HEALTH_NO_EVENTS: f64 = 0.20;
/// Causal confidence for session health observation.
const SESSION_HEALTH_CAUSAL: f64 = 0.20;

/// Evidence confidence for graph-slice derived artifacts.
const GRAPH_EVIDENCE_CONFIDENCE: f64 = 0.80;

/// Classification confidence for action hints with observations.
const HINT_CLASSIFICATION_WITH_OBS: f64 = 0.75;
/// Classification confidence for action hints without observations.
const HINT_CLASSIFICATION_NO_OBS: f64 = 0.50;
/// Evidence confidence for action hints with observations.
const HINT_EVIDENCE_WITH_OBS: f64 = 0.70;
/// Evidence confidence for action hints without observations.
const HINT_EVIDENCE_NO_OBS: f64 = 0.45;

/// Max characters for evidence/action-hint summaries.
const _EVIDENCE_SUMMARY_MAX_CHARS: usize = 180;
/// Max graph nodes to promote to evidence.
const _GRAPH_EVIDENCE_MAX_NODES: usize = 50;
/// Max action hints to include.
const MAX_ACTION_HINTS: usize = 8;

pub(super) fn build_observation_envelope(
    session_id: &str,
    request: &ReflectRequest,
    overview: &SessionOverview,
    diagnoses: &[Diagnosis],
    insights: &[Insight],
    recommendations: &[ReflectRecommendation],
    evidence_graph: Option<&ObservationGraphSlice>,
) -> (
    String,
    Vec<ObservationRecord>,
    Vec<ObservationEvidence>,
    Vec<ObservationActionHint>,
    Vec<ObservationFailureCluster>,
) {
    let summary = build_reflect_summary(overview, diagnoses, insights);

    let (mut observations, mut evidence, failure_clusters) =
        build_diagnosis_observations(diagnoses, session_id);
    let (insight_obs, insight_ev) = build_insight_observations(insights, session_id);
    observations.extend(insight_obs);
    evidence.extend(insight_ev);

    if observations.is_empty() {
        observations.push(build_fallback_session_observation(
            &summary, overview, request, session_id,
        ));
    }

    evidence.extend(build_graph_evidence(evidence_graph));

    let action_hints =
        build_action_hints_from_recommendations(recommendations, &observations, session_id);

    (
        summary,
        observations,
        evidence,
        action_hints,
        failure_clusters,
    )
}

// ── Observation builders (split from build_observation_envelope) ─────────────

fn build_diagnosis_observations(
    diagnoses: &[Diagnosis],
    session_id: &str,
) -> (
    Vec<ObservationRecord>,
    Vec<ObservationEvidence>,
    Vec<ObservationFailureCluster>,
) {
    let mut observations = Vec::new();
    let mut evidence = Vec::new();
    let mut failure_clusters = Vec::new();

    for (idx, diagnosis) in diagnoses.iter().enumerate() {
        let observation_ref = diagnosis_observation_ref(session_id, diagnosis);
        let mut evidence_refs = Vec::new();
        for (sample_idx, sample) in diagnosis.samples.iter().enumerate() {
            let evidence_ref = Urn::new("artifact", "cloud", "reflect")
                .seg(session_id)
                .seg("diagnosis")
                .idx(idx)
                .seg("sample")
                .idx(sample_idx)
                .build();
            evidence_refs.push(evidence_ref.clone());
            evidence.push(ObservationEvidence {
                ref_id: evidence_ref,
                evidence_class: "observed_evidence".to_string(),
                source: "agent_events.error_sample".to_string(),
                summary: summary_truncated(sample, _EVIDENCE_SUMMARY_MAX_CHARS),
                confidence: ObservationConfidence::evidence(diagnosis_evidence_confidence(
                    diagnosis,
                )),
            });
        }

        let (topic, facet) = diagnosis_topic_facet();
        observations.push(ObservationRecord {
            ref_id: observation_ref.clone(),
            topic,
            facet,
            kind: format!("diagnosis:{}", diagnosis.category),
            severity: diagnosis.severity.clone(),
            summary: diagnosis.summary.clone(),
            confidence: ObservationConfidence::complete(
                DIAGNOSIS_COMPLETENESS,
                diagnosis_evidence_confidence(diagnosis),
                diagnosis_causal_confidence(diagnosis),
            ),
            evidence_refs,
        });
        failure_clusters.push(ObservationFailureCluster {
            cluster_ref: Urn::new("failure_cluster", "graph", "reflect")
                .seg(session_id)
                .seg(&diagnosis.category.to_string())
                .seg(&diagnosis.affected_tool)
                .build(),
            label: format!(
                "{}_{}",
                diagnosis.category,
                urn_component(&diagnosis.affected_tool)
            ),
            summary: format!(
                "{} affected {} {} time{}",
                diagnosis.category,
                diagnosis.affected_tool,
                diagnosis.occurrences,
                if diagnosis.occurrences == 1 { "" } else { "s" }
            ),
            observation_refs: vec![observation_ref],
            evidence_class: "inferred_evidence".to_string(),
            confidence: ObservationConfidence::classification_evidence(
                FAILURE_CLUSTER_CLASSIFICATION,
                diagnosis_evidence_confidence(diagnosis),
            ),
        });
    }

    (observations, evidence, failure_clusters)
}

fn build_insight_observations(
    insights: &[Insight],
    session_id: &str,
) -> (Vec<ObservationRecord>, Vec<ObservationEvidence>) {
    let mut observations = Vec::new();
    let mut evidence = Vec::new();

    for (idx, insight) in insights.iter().enumerate() {
        let observation_ref = insight_observation_ref(session_id, &insight.kind);
        let mut evidence_refs = Vec::new();
        if !insight.evidence.trim().is_empty() {
            let evidence_ref = Urn::new("artifact", "cloud", "reflect")
                .seg(session_id)
                .seg("insight")
                .idx(idx)
                .seg("evidence")
                .build();
            evidence_refs.push(evidence_ref.clone());
            evidence.push(ObservationEvidence {
                ref_id: evidence_ref,
                evidence_class: "inferred_evidence".to_string(),
                source: "reflect.statistical_insight".to_string(),
                summary: summary_truncated(&insight.evidence, _EVIDENCE_SUMMARY_MAX_CHARS),
                confidence: ObservationConfidence::evidence(INSIGHT_EVIDENCE_CONFIDENCE),
            });
        }

        let (topic, facet) = insight_topic_facet(insight);
        observations.push(ObservationRecord {
            ref_id: observation_ref,
            topic,
            facet,
            kind: format!("insight:{}", insight.kind.as_str()),
            severity: insight.severity.clone(),
            summary: insight.message.clone(),
            confidence: ObservationConfidence::complete(
                INSIGHT_COMPLETENESS_WITH_EV,
                if evidence_refs.is_empty() {
                    INSIGHT_COMPLETENESS_NO_EV
                } else {
                    INSIGHT_EVIDENCE_CONFIDENCE
                },
                INSIGHT_CAUSAL,
            ),
            evidence_refs,
        });
    }

    (observations, evidence)
}

fn build_fallback_session_observation(
    summary: &str,
    overview: &SessionOverview,
    request: &ReflectRequest,
    session_id: &str,
) -> ObservationRecord {
    ObservationRecord {
        ref_id: Urn::new("observation", "graph", "reflect")
            .seg(session_id)
            .seg("session")
            .seg("health")
            .build(),
        topic: request.topic.as_str().to_string(),
        facet: request.facet.as_str().to_string(),
        kind: "session_health".to_string(),
        severity: if overview.error_count == 0 {
            "info".to_string()
        } else {
            "warning".to_string()
        },
        summary: summary.to_string(),
        confidence: ObservationConfidence::complete(
            SESSION_HEALTH_COMPLETENESS,
            if overview.total_events > 0 {
                SESSION_HEALTH_EVENTS_PRESENT
            } else {
                SESSION_HEALTH_NO_EVENTS
            },
            SESSION_HEALTH_CAUSAL,
        ),
        evidence_refs: Vec::new(),
    }
}

fn build_graph_evidence(
    evidence_graph: Option<&ObservationGraphSlice>,
) -> Vec<ObservationEvidence> {
    let mut evidence = Vec::new();
    if let Some(graph) = evidence_graph {
        for node in graph.nodes.iter().take(50) {
            evidence.push(ObservationEvidence {
                ref_id: node.ref_id.clone(),
                evidence_class: "observed_evidence".to_string(),
                source: "reflect.graph_slice".to_string(),
                summary: summary_truncated(
                    node.summary
                        .as_deref()
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or(&node.label),
                    _EVIDENCE_SUMMARY_MAX_CHARS,
                ),
                confidence: ObservationConfidence::evidence(GRAPH_EVIDENCE_CONFIDENCE),
            });
        }
    }
    evidence
}

fn build_action_hints_from_recommendations(
    recommendations: &[ReflectRecommendation],
    observations: &[ObservationRecord],
    session_id: &str,
) -> Vec<ObservationActionHint> {
    recommendations
        .iter()
        .filter(|recommendation| !recommendation.summary.trim().is_empty())
        .take(MAX_ACTION_HINTS)
        .map(|recommendation| {
            let observation_refs = observation_ref_for_recommendation(recommendation, session_id)
                .filter(|ref_id| {
                    observations
                        .iter()
                        .any(|observation| observation.ref_id == *ref_id)
                })
                .into_iter()
                .collect::<Vec<_>>();
            ObservationActionHint {
                target_type: "user_guidance".to_string(),
                summary: recommendation.summary.trim().to_string(),
                confidence: ObservationConfidence::classification_evidence(
                    if observation_refs.is_empty() {
                        HINT_CLASSIFICATION_NO_OBS
                    } else {
                        HINT_CLASSIFICATION_WITH_OBS
                    },
                    if observation_refs.is_empty() {
                        HINT_EVIDENCE_NO_OBS
                    } else {
                        HINT_EVIDENCE_WITH_OBS
                    },
                ),
                observation_refs,
            }
        })
        .collect()
}

fn diagnosis_topic_facet() -> (String, String) {
    ("execution".to_string(), "errors".to_string())
}

fn insight_topic_facet(insight: &Insight) -> (String, String) {
    match insight.kind {
        InsightKind::ErrorRate => ("execution".to_string(), "errors".to_string()),
        InsightKind::ToolFailure { .. } | InsightKind::ToolConcentration { .. } => {
            ("execution".to_string(), "tools".to_string())
        }
        InsightKind::DecisionStall => ("execution".to_string(), "stall".to_string()),
        InsightKind::ModelFanout { .. } | InsightKind::EmptySession => {
            ("runtime".to_string(), "performance".to_string())
        }
    }
}

fn diagnosis_observation_ref(session_id: &str, diagnosis: &Diagnosis) -> String {
    Urn::new("observation", "graph", "reflect")
        .seg(session_id)
        .seg("diagnosis")
        .seg(diagnosis.category.as_str())
        .seg(&diagnosis.affected_tool)
        .build()
}

fn insight_observation_ref(session_id: &str, kind: &InsightKind) -> String {
    let key = match kind {
        InsightKind::ErrorRate => "error_rate".to_string(),
        InsightKind::ToolFailure { tool } => format!("tool_failure:{tool}"),
        InsightKind::ToolConcentration { tool } => format!("tool_concentration:{tool}"),
        InsightKind::ModelFanout { decision_type } => format!("model_fanout:{decision_type}"),
        InsightKind::EmptySession => "empty_session".to_string(),
        InsightKind::DecisionStall => "decision_stall".to_string(),
    };
    Urn::new("observation", "graph", "reflect")
        .seg(session_id)
        .seg("insight")
        .seg(&key)
        .build()
}

fn observation_ref_for_recommendation(
    recommendation: &ReflectRecommendation,
    session_id: &str,
) -> Option<String> {
    match &recommendation.source {
        ReflectRecommendationSource::Diagnosis {
            category,
            affected_tool,
        } => Some(
            Urn::new("observation", "graph", "reflect")
                .seg(session_id)
                .seg("diagnosis")
                .seg(category.as_str())
                .seg(affected_tool)
                .build(),
        ),
        ReflectRecommendationSource::Insight(kind) => {
            Some(insight_observation_ref(session_id, kind))
        }
        ReflectRecommendationSource::Session => None,
    }
}

fn build_reflect_summary(
    overview: &SessionOverview,
    diagnoses: &[Diagnosis],
    insights: &[Insight],
) -> String {
    if let Some(diagnosis) = diagnoses
        .iter()
        .find(|diagnosis| diagnosis.severity == "critical")
        .or_else(|| {
            diagnoses
                .iter()
                .find(|diagnosis| diagnosis.severity == "warning")
        })
    {
        return diagnosis.summary.clone();
    }

    // An isolated, low-severity error is still an observed diagnosis. Do not
    // erase that evidence behind the generic "no high-confidence root cause"
    // message: the latter describes causal certainty, not whether the error
    // happened. Keep the wording explicit so consumers do not mistake an
    // informational classification for a proven root cause.
    if let Some(diagnosis) = diagnoses.first() {
        return format!(
            "Observed issue (causal confidence is limited): {}",
            diagnosis.summary
        );
    }

    if let Some(insight) = insights
        .iter()
        .find(|insight| insight.severity == "critical")
        .or_else(|| {
            insights
                .iter()
                .find(|insight| insight.severity == "warning")
        })
    {
        return insight.message.clone();
    }

    if overview.total_events == 0 {
        "Empty session - no events recorded yet".to_string()
    } else if overview.error_count == 0 {
        format!(
            "Session healthy - {} events and {} decisions with no errors detected",
            overview.total_events, overview.total_decisions
        )
    } else {
        format!(
            "Session has {} errors across {} events, but no high-confidence root cause was found",
            overview.error_count, overview.total_events
        )
    }
}

fn diagnosis_evidence_confidence(diagnosis: &Diagnosis) -> f64 {
    if diagnosis.samples.is_empty() {
        DIAGNOSIS_LOW_CONFIDENCE
    } else if diagnosis.occurrences >= 3 {
        DIAGNOSIS_HIGH_CONFIDENCE
    } else {
        DIAGNOSIS_MEDIUM_CONFIDENCE
    }
}

fn diagnosis_causal_confidence(diagnosis: &Diagnosis) -> f64 {
    match diagnosis.severity.as_str() {
        "critical" => CAUSAL_CRITICAL_CONFIDENCE,
        "warning" => CAUSAL_WARNING_CONFIDENCE,
        _ => CAUSAL_DEFAULT_CONFIDENCE,
    }
}

fn summary_truncated(value: &str, max_chars: usize) -> String {
    let mut out: String = value.chars().take(max_chars).collect();
    if value.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

pub(super) fn graph_event_ref(event_id: &str) -> String {
    Urn::new("event", "cloud", event_id).build()
}

pub(super) fn graph_decision_ref(decision_id: &str) -> String {
    Urn::new("decision", "cloud", decision_id).build()
}

#[cfg(test)]
mod summary_tests {
    use super::*;
    use crate::reflect::{Diagnosis, SessionOverview};

    fn overview(error_count: i64) -> SessionOverview {
        SessionOverview {
            total_events: 10,
            total_decisions: 2,
            duration_minutes: Some(1.0),
            unique_skills_used: 1,
            error_count,
            error_rate_pct: error_count as f64 * 10.0,
            top_event_types: Vec::new(),
            top_skills: vec![("agent_fanout".into(), 1)],
        }
    }

    #[test]
    fn isolated_info_diagnosis_is_not_hidden_by_generic_summary() {
        let summary = build_reflect_summary(
            &overview(1),
            &[Diagnosis {
                category: astra_core::ErrorKind::ToolInvalidArgs,
                severity: "info".into(),
                summary:
                    "Tool parameter errors (agent_fanout): wrong arguments passed — 1 occurrences"
                        .into(),
                samples: vec!["unknown field tools".into()],
                occurrences: 1,
                affected_tool: "agent_fanout".into(),
                fix_hint: "use the typed schema".into(),
            }],
            &[],
        );
        assert!(summary.contains("Observed issue"), "{summary}");
        assert!(summary.contains("agent_fanout"), "{summary}");
        assert!(
            !summary.contains("no high-confidence root cause"),
            "{summary}"
        );
    }
}
