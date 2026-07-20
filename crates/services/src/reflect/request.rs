use std::collections::BTreeMap;

use astra_core::{
    ObservationDepth, ObservationFacet, ObservationHorizon, ObservationProviderCoverage,
    ObservationTopic, SourcePolicy,
};

use super::{ObservationDataCoverage, ObservationView};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflectRequest {
    pub topic: ObservationTopic,
    pub facet: ObservationFacet,
    pub depth: ObservationDepth,
    pub horizon: ObservationHorizon,
    pub source_policy: SourcePolicy,
    pub include_context: bool,
    pub analysis_view: String,
    pub last_n: i32,
    pub question: String,
}

impl ReflectRequest {
    pub fn from_observation_params(
        topic: Option<&str>,
        facet: Option<&str>,
        depth: Option<&str>,
        horizon: Option<&str>,
        last_n: i32,
        question: &str,
    ) -> Self {
        Self::from_observation_params_with_source(
            topic, facet, depth, horizon, None, false, last_n, question,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_observation_params_with_source(
        topic: Option<&str>,
        facet: Option<&str>,
        depth: Option<&str>,
        horizon: Option<&str>,
        source_policy: Option<&str>,
        include_context: bool,
        last_n: i32,
        question: &str,
    ) -> Self {
        let topic_supplied = topic.is_some_and(|t| !t.trim().is_empty());
        let (topic_head, topic_facet) = split_reflect_topic(topic.unwrap_or("overview"));
        let mut topic = ObservationTopic::from_arg(topic_head);
        let facet_candidate = facet
            .or(topic_facet)
            .unwrap_or_else(|| default_reflect_facet_for_topic(topic));
        let (facet_topic, facet_tail) = split_reflect_topic(facet_candidate);
        if !topic_supplied {
            topic = if facet_tail.is_some() {
                ObservationTopic::from_arg(facet_topic)
            } else {
                reflect_topic_for_standalone_facet(facet_candidate).unwrap_or(topic)
            };
        }
        let facet = normalize_reflect_facet(facet_tail.unwrap_or(facet_candidate));
        let analysis_view = analysis_view_for_topic_facet(topic, facet);

        Self {
            topic,
            facet,
            depth: ObservationDepth::from_arg(depth.unwrap_or("diagnostic")),
            horizon: ObservationHorizon::from_arg(horizon.unwrap_or("session")),
            source_policy: SourcePolicy::from_arg(source_policy.unwrap_or("auto")),
            include_context,
            analysis_view,
            last_n: normalize_last_n(last_n),
            question: question.trim().to_string(),
        }
    }

    pub fn decision_trace(last_n: i32, question: &str) -> Self {
        Self {
            topic: ObservationTopic::Execution,
            facet: ObservationFacet::Trace,
            depth: ObservationDepth::Diagnostic,
            horizon: ObservationHorizon::Session,
            source_policy: SourcePolicy::Auto,
            include_context: false,
            analysis_view: "execution_trace".to_string(),
            last_n: normalize_last_n(last_n),
            question: question.trim().to_string(),
        }
    }

    pub(super) fn view(&self, total_events: i64, total_decisions: i64) -> ObservationView {
        let mut warnings = Vec::new();
        if matches!(
            self.horizon,
            ObservationHorizon::Now | ObservationHorizon::CurrentTurn
        ) {
            warnings.push(
                "reflect is backed by persisted session evidence; current-turn data may lag until events are stored"
                    .to_string(),
            );
        }
        if self.depth == ObservationDepth::Forensic && self.last_n > 50 {
            warnings.push("forensic evidence graph is bounded to recent decisions".to_string());
        }
        if self.include_context {
            warnings.push(
                "include_context requested, but this server view only includes persisted reflection context"
                    .to_string(),
            );
        }
        if matches!(
            self.source_policy,
            SourcePolicy::LiveOnly | SourcePolicy::LocalOnly
        ) {
            warnings.push(
                "requested source policy is not fully satisfiable from the server database"
                    .to_string(),
            );
        }

        ObservationView {
            topic: self.topic.as_str().to_string(),
            facet: self.facet.as_str().to_string(),
            depth: self.depth.as_str().to_string(),
            horizon: self.horizon.as_str().to_string(),
            data_coverage: ObservationDataCoverage {
                overall: if warnings.is_empty() {
                    "fresh".to_string()
                } else {
                    "partial".to_string()
                },
                source: reflect_source_label(self.source_policy).to_string(),
                events: total_events,
                decisions: total_decisions,
                providers: reflect_providers(self.source_policy, self.include_context),
                warnings,
            },
        }
    }
}

fn reflect_providers(
    source_policy: SourcePolicy,
    include_context: bool,
) -> BTreeMap<String, ObservationProviderCoverage> {
    let mut providers = BTreeMap::new();
    providers.insert(
        "cloud_events".to_string(),
        ObservationProviderCoverage {
            status: "fresh".to_string(),
            freshness_ms: None,
            reason: None,
        },
    );
    providers.insert(
        "decision_audits".to_string(),
        ObservationProviderCoverage {
            status: "fresh".to_string(),
            freshness_ms: None,
            reason: None,
        },
    );
    if matches!(
        source_policy,
        SourcePolicy::LiveOnly | SourcePolicy::LocalOnly
    ) {
        providers.insert(
            "local_journal".to_string(),
            ObservationProviderCoverage {
                status: "missing".to_string(),
                freshness_ms: None,
                reason: Some("not_available_from_server_db".to_string()),
            },
        );
    }
    if include_context {
        providers.insert(
            "visible_context".to_string(),
            ObservationProviderCoverage {
                status: "missing".to_string(),
                freshness_ms: None,
                reason: Some("provider_not_attached".to_string()),
            },
        );
    }
    providers
}

fn split_reflect_topic(topic: &str) -> (&str, Option<&str>) {
    topic
        .split_once('/')
        .map(|(head, tail)| (head.trim(), Some(tail.trim())))
        .unwrap_or((topic.trim(), None))
}

fn normalize_last_n(last_n: i32) -> i32 {
    last_n.clamp(1, 100)
}

fn reflect_source_label(source_policy: SourcePolicy) -> &'static str {
    match source_policy {
        SourcePolicy::CloudOnly => "server_db_cloud_only",
        SourcePolicy::DurableFirst => "server_db_durable_first",
        SourcePolicy::LiveFirst => "server_db_live_first",
        SourcePolicy::LiveOnly => "server_db_live_only_unavailable",
        SourcePolicy::LocalOnly => "server_db_local_only_unavailable",
        SourcePolicy::Auto => "server_db",
    }
}

fn default_reflect_facet_for_topic(topic: ObservationTopic) -> &'static str {
    match topic {
        ObservationTopic::Runtime => "performance",
        ObservationTopic::Execution => "tools",
        ObservationTopic::Knowledge => "context",
        ObservationTopic::Overview => "overview",
    }
}

fn reflect_topic_for_standalone_facet(facet: &str) -> Option<ObservationTopic> {
    match facet.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "errors" | "tools" | "trace" | "recent" => Some(ObservationTopic::Execution),
        "performance" | "runtime" | "cache" | "stall" | "noise" => Some(ObservationTopic::Runtime),
        "context" | "memory" | "session_memory" => Some(ObservationTopic::Knowledge),
        "overview" | "all" => Some(ObservationTopic::Overview),
        _ => None,
    }
}

fn normalize_reflect_facet(facet: &str) -> ObservationFacet {
    match facet.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        // Reflect's persisted-view vocabulary maps onto the shared typed
        // observation facets. Keep the mapping here instead of letting valid
        // schema values silently fall back to Session.
        "performance" => ObservationFacet::Session,
        "tools" => ObservationFacet::Recent,
        "context" => ObservationFacet::Overview,
        "memory" => ObservationFacet::SessionMemory,
        normalized => normalized.parse().unwrap_or(ObservationFacet::Session),
    }
}

fn analysis_view_for_topic_facet(topic: ObservationTopic, facet: ObservationFacet) -> String {
    // facet is already normalized — no '/' separator can appear
    match (topic, facet) {
        (ObservationTopic::Execution, ObservationFacet::Errors) => "execution_errors",
        (ObservationTopic::Execution, ObservationFacet::Trace) => "execution_trace",
        (ObservationTopic::Execution, _) => "execution_tools",
        (ObservationTopic::Knowledge, _) => "knowledge_context",
        (ObservationTopic::Runtime, _) => "runtime_performance",
        (ObservationTopic::Overview, _) => "overview",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_facet_maps_to_analysis_view() {
        let request = ReflectRequest::from_observation_params(
            Some("execution"),
            Some("trace"),
            Some("forensic"),
            Some("recent"),
            50,
            "",
        );
        assert_eq!(request.topic, ObservationTopic::Execution);
        assert_eq!(request.facet, ObservationFacet::Trace);
        assert_eq!(request.analysis_view, "execution_trace");
        assert_eq!(request.depth, ObservationDepth::Forensic);
        assert_eq!(request.horizon, ObservationHorizon::Recent);
        assert_eq!(request.source_policy, SourcePolicy::Auto);
        assert!(!request.include_context);
    }

    #[test]
    fn source_policy_and_context_are_carried_into_view_coverage() {
        let request = ReflectRequest::from_observation_params_with_source(
            Some("knowledge"),
            Some("session_memory"),
            Some("hint"),
            Some("session"),
            Some("local_only"),
            true,
            20,
            "",
        );

        assert_eq!(request.topic, ObservationTopic::Knowledge);
        assert_eq!(request.facet, ObservationFacet::SessionMemory);
        assert_eq!(request.depth, ObservationDepth::Hint);
        assert_eq!(request.source_policy, SourcePolicy::LocalOnly);
        assert!(request.include_context);

        let view = request.view(10, 2);
        assert_eq!(
            view.data_coverage.source,
            "server_db_local_only_unavailable"
        );
        assert!(
            view.data_coverage
                .warnings
                .iter()
                .any(|warning| warning.contains("not fully satisfiable")),
            "expected source-policy warning: {:?}",
            view.data_coverage.warnings
        );
        assert!(
            view.data_coverage
                .warnings
                .iter()
                .any(|warning| warning.contains("include_context requested")),
            "expected context warning: {:?}",
            view.data_coverage.warnings
        );
    }

    #[test]
    fn normalized_topic_facet_determines_analysis_view() {
        let request = ReflectRequest::from_observation_params(
            Some("execution"),
            Some("trace"),
            Some("forensic"),
            Some("session"),
            20,
            "show the trace",
        );
        assert_eq!(request.topic, ObservationTopic::Execution);
        assert_eq!(request.facet, ObservationFacet::Trace);
        assert_eq!(request.analysis_view, "execution_trace");
        assert_eq!(request.depth, ObservationDepth::Forensic);
        assert_eq!(request.horizon, ObservationHorizon::Session);
        assert_eq!(request.question, "show the trace");
    }

    #[test]
    fn default_request_is_overview_without_legacy_input() {
        let request = ReflectRequest::from_observation_params(
            None,
            None,
            Some("summary"),
            Some("recent"),
            12,
            "why failed?",
        );
        assert_eq!(request.topic, ObservationTopic::Overview);
        assert_eq!(request.facet, ObservationFacet::Overview);
        assert_eq!(request.analysis_view, "overview");
        assert_eq!(request.depth, ObservationDepth::Summary);
        assert_eq!(request.horizon, ObservationHorizon::Recent);
        assert_eq!(request.last_n, 12);
    }

    #[test]
    fn removed_focus_shortcuts_do_not_map_to_error_analysis() {
        let request = ReflectRequest::from_observation_params(
            Some("execution"),
            Some("skill_failure"),
            Some("summary"),
            Some("recent"),
            12,
            "",
        );
        assert_eq!(request.topic, ObservationTopic::Execution);
        assert_eq!(request.facet.as_str(), "session"); // unknown facets fallback to Session
        assert_eq!(request.analysis_view, "execution_tools");
    }

    #[test]
    fn removed_minimal_depth_alias_defaults_to_diagnostic() {
        let request = ReflectRequest::from_observation_params(
            Some("execution"),
            Some("errors"),
            Some("minimal"),
            None,
            20,
            "",
        );
        assert_eq!(request.topic, ObservationTopic::Execution);
        assert_eq!(request.facet, ObservationFacet::Errors);
        assert_eq!(request.depth, ObservationDepth::Summary); // "minimal" is no longer an alias
        assert_eq!(request.analysis_view, "execution_errors");
    }

    #[test]
    fn infers_topic_from_slash_facet() {
        let request = ReflectRequest::from_observation_params(
            None,
            Some("execution/errors"),
            None,
            None,
            20,
            "",
        );
        assert_eq!(request.topic, ObservationTopic::Execution);
        assert_eq!(request.facet, ObservationFacet::Errors);
        assert_eq!(request.analysis_view, "execution_errors");
    }

    #[test]
    fn standalone_error_facet_infers_execution_evidence() {
        let request = ReflectRequest::from_observation_params(
            None,
            Some("errors"),
            None,
            None,
            20,
            "why did the tool fail?",
        );
        assert_eq!(request.topic, ObservationTopic::Execution);
        assert_eq!(request.facet, ObservationFacet::Errors);
        assert_eq!(request.analysis_view, "execution_errors");
    }

    #[test]
    fn every_advertised_reflect_facet_maps_to_a_typed_view() {
        let cases = [
            (
                "performance",
                ObservationTopic::Runtime,
                "runtime_performance",
            ),
            ("tools", ObservationTopic::Execution, "execution_tools"),
            ("context", ObservationTopic::Knowledge, "knowledge_context"),
            ("memory", ObservationTopic::Knowledge, "knowledge_context"),
        ];
        for (facet, topic, analysis_view) in cases {
            let request =
                ReflectRequest::from_observation_params(None, Some(facet), None, None, 20, "");
            assert_eq!(request.topic, topic, "facet={facet}");
            assert_eq!(request.analysis_view, analysis_view, "facet={facet}");
        }
    }

    #[test]
    fn decision_trace_uses_trace_analysis_view() {
        let request = ReflectRequest::decision_trace(10, "why this decision?");
        assert_eq!(request.topic, ObservationTopic::Execution);
        assert_eq!(request.facet, ObservationFacet::Trace);
        assert_eq!(request.analysis_view, "execution_trace");
        assert_eq!(request.depth, ObservationDepth::Diagnostic);
        assert_eq!(request.horizon, ObservationHorizon::Session);
        assert_eq!(request.last_n, 10);
        assert_eq!(request.question, "why this decision?");
    }

    #[test]
    fn evidence_budget_is_clamped_at_request_boundary() {
        let too_large = ReflectRequest::from_observation_params(
            Some("execution"),
            Some("errors"),
            None,
            None,
            250,
            "",
        );
        assert_eq!(too_large.last_n, 100);

        let too_small = ReflectRequest::decision_trace(0, "");
        assert_eq!(too_small.last_n, 1);
    }
}
