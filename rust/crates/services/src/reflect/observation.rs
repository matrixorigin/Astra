use super::{
    Diagnosis, Insight, ObservationActionHint, ObservationConfidence, ObservationEvidence,
    ObservationFailureCluster, ObservationRecord, ReflectRequest, SessionOverview,
};
use astra_core::ObservationGraphSlice;

pub(super) struct ObservationEnvelope {
    pub summary: String,
    pub observations: Vec<ObservationRecord>,
    pub evidence: Vec<ObservationEvidence>,
    pub action_hints: Vec<ObservationActionHint>,
    pub failure_clusters: Vec<ObservationFailureCluster>,
}

pub(super) fn build_observation_envelope(
    session_id: &str,
    request: &ReflectRequest,
    overview: &SessionOverview,
    diagnoses: &[Diagnosis],
    insights: &[Insight],
    recommendations: &[String],
    evidence_graph: Option<&ObservationGraphSlice>,
) -> ObservationEnvelope {
    let summary = build_reflect_summary(overview, diagnoses, insights);
    let session_component = urn_component(session_id);
    let mut observations = Vec::new();
    let mut evidence = Vec::new();
    let mut failure_clusters = Vec::new();

    for (idx, diagnosis) in diagnoses.iter().enumerate() {
        let observation_ref =
            format!("urn:astra:observation:graph:reflect:{session_component}:diagnosis:{idx}");
        let mut evidence_refs = Vec::new();
        for (sample_idx, sample) in diagnosis.samples.iter().enumerate() {
            let evidence_ref = format!(
                "urn:astra:artifact:cloud:reflect:{session_component}:diagnosis:{idx}:sample:{sample_idx}"
            );
            evidence_refs.push(evidence_ref.clone());
            evidence.push(ObservationEvidence {
                ref_id: evidence_ref,
                evidence_class: "observed_evidence".to_string(),
                source: "agent_events.error_sample".to_string(),
                summary: truncate_chars(sample, 180),
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
                0.90,
                diagnosis_evidence_confidence(diagnosis),
                diagnosis_causal_confidence(diagnosis),
            ),
            evidence_refs,
        });
        failure_clusters.push(ObservationFailureCluster {
            cluster_ref: format!(
                "urn:astra:failure_cluster:graph:reflect:{session_component}:{}:{}",
                urn_component(&diagnosis.category.to_string()),
                urn_component(&diagnosis.affected_tool)
            ),
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
                0.84,
                diagnosis_evidence_confidence(diagnosis),
            ),
        });
    }

    for (idx, insight) in insights.iter().enumerate() {
        let observation_ref =
            format!("urn:astra:observation:graph:reflect:{session_component}:insight:{idx}");
        let mut evidence_refs = Vec::new();
        if !insight.evidence.trim().is_empty() {
            let evidence_ref = format!(
                "urn:astra:artifact:cloud:reflect:{session_component}:insight:{idx}:evidence"
            );
            evidence_refs.push(evidence_ref.clone());
            evidence.push(ObservationEvidence {
                ref_id: evidence_ref,
                evidence_class: "inferred_evidence".to_string(),
                source: "reflect.statistical_insight".to_string(),
                summary: truncate_chars(&insight.evidence, 180),
                confidence: ObservationConfidence::evidence(0.70),
            });
        }

        let (topic, facet) = insight_topic_facet(request, insight);
        observations.push(ObservationRecord {
            ref_id: observation_ref,
            topic,
            facet,
            kind: format!("insight:{}", insight.category),
            severity: insight.severity.clone(),
            summary: insight.message.clone(),
            confidence: ObservationConfidence::complete(
                0.80,
                if evidence_refs.is_empty() { 0.45 } else { 0.70 },
                0.40,
            ),
            evidence_refs,
        });
    }

    if observations.is_empty() {
        observations.push(ObservationRecord {
            ref_id: format!(
                "urn:astra:observation:graph:reflect:{session_component}:session:health"
            ),
            topic: request.topic.clone(),
            facet: request.facet.clone(),
            kind: "session_health".to_string(),
            severity: if overview.error_count == 0 {
                "info".to_string()
            } else {
                "warning".to_string()
            },
            summary: summary.clone(),
            confidence: ObservationConfidence::complete(
                0.70,
                if overview.total_events > 0 {
                    0.65
                } else {
                    0.20
                },
                0.20,
            ),
            evidence_refs: Vec::new(),
        });
    }

    if let Some(graph) = evidence_graph {
        for node in graph.nodes.iter().take(50) {
            evidence.push(ObservationEvidence {
                ref_id: node.ref_id.clone(),
                evidence_class: "observed_evidence".to_string(),
                source: "reflect.graph_slice".to_string(),
                summary: truncate_chars(
                    node.summary
                        .as_deref()
                        .filter(|summary| !summary.trim().is_empty())
                        .unwrap_or(&node.label),
                    180,
                ),
                confidence: ObservationConfidence::evidence(0.80),
            });
        }
    }

    let action_hints = recommendations
        .iter()
        .filter(|recommendation| !recommendation.trim().is_empty())
        .take(8)
        .map(|recommendation| {
            let observation_refs = observation_refs_for_recommendation(
                recommendation,
                observations.as_slice(),
                diagnoses,
                insights,
                &session_component,
            );
            ObservationActionHint {
                target_type: "user_guidance".to_string(),
                summary: recommendation.trim().to_string(),
                confidence: ObservationConfidence::classification_evidence(
                    if observation_refs.is_empty() {
                        0.50
                    } else {
                        0.75
                    },
                    if observation_refs.is_empty() {
                        0.45
                    } else {
                        0.70
                    },
                ),
                observation_refs,
            }
        })
        .collect::<Vec<_>>();

    ObservationEnvelope {
        summary,
        observations,
        evidence,
        action_hints,
        failure_clusters,
    }
}

fn diagnosis_topic_facet() -> (String, String) {
    ("execution".to_string(), "errors".to_string())
}

fn insight_topic_facet(request: &ReflectRequest, insight: &Insight) -> (String, String) {
    match insight.category.as_str() {
        "error_pattern" => ("execution".to_string(), "errors".to_string()),
        "tool_usage" => ("execution".to_string(), "tools".to_string()),
        "stall" => ("execution".to_string(), "stall".to_string()),
        "performance" => ("runtime".to_string(), "performance".to_string()),
        _ => (request.topic.clone(), request.facet.clone()),
    }
}

fn observation_refs_for_recommendation(
    recommendation: &str,
    observations: &[ObservationRecord],
    diagnoses: &[Diagnosis],
    insights: &[Insight],
    session_component: &str,
) -> Vec<String> {
    let recommendation = recommendation.trim();
    let normalized = normalize_recommendation(recommendation);
    let mut refs = Vec::new();

    for (idx, diagnosis) in diagnoses.iter().enumerate() {
        if !diagnosis.fix_hint.trim().is_empty()
            && normalize_recommendation(&diagnosis.fix_hint) == normalized
        {
            refs.push(format!(
                "urn:astra:observation:graph:reflect:{session_component}:diagnosis:{idx}"
            ));
        }
    }

    if refs.is_empty() {
        for (idx, insight) in insights.iter().enumerate() {
            if recommendation_matches_insight(recommendation, insight) {
                refs.push(format!(
                    "urn:astra:observation:graph:reflect:{session_component}:insight:{idx}"
                ));
            }
        }
    }

    refs.retain(|ref_id| {
        observations
            .iter()
            .any(|observation| observation.ref_id == *ref_id)
    });
    refs.truncate(5);
    refs
}

fn normalize_recommendation(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn recommendation_matches_insight(recommendation: &str, insight: &Insight) -> bool {
    let recommendation = normalize_recommendation(recommendation);
    let message = normalize_recommendation(&insight.message);
    match insight.category.as_str() {
        "stall" => recommendation.contains("events without decisions"),
        "tool_usage" => {
            message.contains("over-reliance") && recommendation.contains("diverse tools")
        }
        _ => false,
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
        0.45
    } else if diagnosis.occurrences >= 3 {
        0.90
    } else {
        0.78
    }
}

fn diagnosis_causal_confidence(diagnosis: &Diagnosis) -> f64 {
    match diagnosis.severity.as_str() {
        "critical" => 0.82,
        "warning" => 0.70,
        _ => 0.55,
    }
}

pub(super) fn graph_event_ref(event_id: &str) -> String {
    format!("urn:astra:event:cloud:{}", urn_component(event_id))
}

pub(super) fn graph_decision_ref(decision_id: &str) -> String {
    format!("urn:astra:decision:cloud:{}", urn_component(decision_id))
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

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}
