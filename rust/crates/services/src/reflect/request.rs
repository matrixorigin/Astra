use std::collections::BTreeMap;

use astra_core::ObservationProviderCoverage;

use super::{ObservationDataCoverage, ObservationView};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflectRequest {
    pub topic: String,
    pub facet: String,
    pub depth: String,
    pub horizon: String,
    pub source_policy: String,
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
        let topic_supplied = topic.is_some_and(|topic| !topic.trim().is_empty());
        let (topic_head, topic_facet) = split_reflect_topic(topic.unwrap_or("overview"));
        let mut topic = normalize_topic(topic_head);
        let facet_candidate = facet
            .or(topic_facet)
            .unwrap_or_else(|| default_reflect_facet_for_topic(topic.as_str()));
        let (facet_topic, facet_tail) = split_reflect_topic(facet_candidate);
        if !topic_supplied && facet_tail.is_some() {
            topic = normalize_topic(facet_topic);
        }
        let facet = normalize_reflect_arg(facet_tail.unwrap_or(facet_candidate));
        let analysis_view = analysis_view_for_topic_facet(&topic, &facet);

        Self {
            topic,
            facet,
            depth: normalize_depth(depth.unwrap_or("diagnostic")),
            horizon: normalize_horizon(horizon.unwrap_or("session")),
            source_policy: normalize_source_policy(source_policy.unwrap_or("auto")),
            include_context,
            analysis_view,
            last_n: normalize_last_n(last_n),
            question: question.trim().to_string(),
        }
    }

    pub fn decision_trace(last_n: i32, question: &str) -> Self {
        Self {
            topic: "execution".to_string(),
            facet: "trace".to_string(),
            depth: "diagnostic".to_string(),
            horizon: "session".to_string(),
            source_policy: "auto".to_string(),
            include_context: false,
            analysis_view: "execution_trace".to_string(),
            last_n: normalize_last_n(last_n),
            question: question.trim().to_string(),
        }
    }

    pub(super) fn view(&self, total_events: i64, total_decisions: i64) -> ObservationView {
        let mut warnings = Vec::new();
        if matches!(self.horizon.as_str(), "now" | "current_turn") {
            warnings.push(
                "reflect is backed by persisted session evidence; current-turn data may lag until events are stored"
                    .to_string(),
            );
        }
        if self.depth == "forensic" && self.last_n > 50 {
            warnings.push("forensic evidence graph is bounded to recent decisions".to_string());
        }
        if self.include_context {
            warnings.push(
                "include_context requested, but this server view only includes persisted reflection context"
                    .to_string(),
            );
        }
        if matches!(self.source_policy.as_str(), "live_only" | "local_only") {
            warnings.push(
                "requested source policy is not fully satisfiable from the server database"
                    .to_string(),
            );
        }

        ObservationView {
            topic: self.topic.clone(),
            facet: self.facet.clone(),
            depth: self.depth.clone(),
            horizon: self.horizon.clone(),
            data_coverage: ObservationDataCoverage {
                overall: if warnings.is_empty() {
                    "fresh".to_string()
                } else {
                    "partial".to_string()
                },
                source: reflect_source_label(&self.source_policy).to_string(),
                events: total_events,
                decisions: total_decisions,
                providers: reflect_providers(&self.source_policy, self.include_context),
                warnings,
            },
        }
    }
}

fn reflect_providers(
    source_policy: &str,
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
    if matches!(source_policy, "live_only" | "local_only") {
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

fn normalize_reflect_arg(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace('-', "_")
}

fn split_reflect_topic(topic: &str) -> (&str, Option<&str>) {
    topic
        .split_once('/')
        .map(|(head, tail)| (head.trim(), Some(tail.trim())))
        .unwrap_or((topic.trim(), None))
}

fn normalize_topic(topic: &str) -> String {
    match normalize_reflect_arg(topic).as_str() {
        "runtime" => "runtime".to_string(),
        "execution" => "execution".to_string(),
        "knowledge" => "knowledge".to_string(),
        _ => "overview".to_string(),
    }
}

fn normalize_depth(depth: &str) -> String {
    match normalize_reflect_arg(depth).as_str() {
        "hint" => "hint".to_string(),
        "summary" => "summary".to_string(),
        "forensic" => "forensic".to_string(),
        _ => "diagnostic".to_string(),
    }
}

fn normalize_horizon(horizon: &str) -> String {
    match normalize_reflect_arg(horizon).as_str() {
        "now" => "now".to_string(),
        "current_turn" => "current_turn".to_string(),
        "recent" => "recent".to_string(),
        "turn" => "turn".to_string(),
        "cross_session" => "cross_session".to_string(),
        _ => "session".to_string(),
    }
}

fn normalize_source_policy(source_policy: &str) -> String {
    match normalize_reflect_arg(source_policy).as_str() {
        "live_only" => "live_only".to_string(),
        "live_first" => "live_first".to_string(),
        "durable_first" => "durable_first".to_string(),
        "local_only" => "local_only".to_string(),
        "cloud_only" => "cloud_only".to_string(),
        _ => "auto".to_string(),
    }
}

fn normalize_last_n(last_n: i32) -> i32 {
    last_n.clamp(1, 100)
}

fn reflect_source_label(source_policy: &str) -> &'static str {
    match source_policy {
        "cloud_only" => "server_db_cloud_only",
        "durable_first" => "server_db_durable_first",
        "live_first" => "server_db_live_first",
        "live_only" => "server_db_live_only_unavailable",
        "local_only" => "server_db_local_only_unavailable",
        _ => "server_db",
    }
}

fn default_reflect_facet_for_topic(topic: &str) -> &'static str {
    match topic {
        "runtime" => "performance",
        "execution" => "tools",
        "knowledge" => "context",
        _ => "overview",
    }
}

fn analysis_view_for_topic_facet(topic: &str, facet: &str) -> String {
    match (topic, facet) {
        ("execution", "errors" | "execution/errors" | "failures") => "execution_errors",
        ("execution", "trace" | "history" | "execution/trace") => "execution_trace",
        ("execution", _) => "execution_tools",
        ("knowledge", "context" | "data_quality" | "memory") => "knowledge_context",
        ("runtime", "performance" | "latency" | "cost") => "runtime_performance",
        _ => "overview",
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
        assert_eq!(request.topic, "execution");
        assert_eq!(request.facet, "trace");
        assert_eq!(request.analysis_view, "execution_trace");
        assert_eq!(request.depth, "forensic");
        assert_eq!(request.horizon, "recent");
        assert_eq!(request.source_policy, "auto");
        assert!(!request.include_context);
    }

    #[test]
    fn source_policy_and_context_are_carried_into_view_coverage() {
        let request = ReflectRequest::from_observation_params_with_source(
            Some("knowledge/context"),
            None,
            Some("hint"),
            Some("session"),
            Some("local_only"),
            true,
            20,
            "",
        );

        assert_eq!(request.topic, "knowledge");
        assert_eq!(request.facet, "context");
        assert_eq!(request.depth, "hint");
        assert_eq!(request.source_policy, "local_only");
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
        assert_eq!(request.topic, "execution");
        assert_eq!(request.facet, "trace");
        assert_eq!(request.analysis_view, "execution_trace");
        assert_eq!(request.depth, "forensic");
        assert_eq!(request.horizon, "session");
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
        assert_eq!(request.topic, "overview");
        assert_eq!(request.facet, "overview");
        assert_eq!(request.analysis_view, "overview");
        assert_eq!(request.depth, "summary");
        assert_eq!(request.horizon, "recent");
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
        assert_eq!(request.topic, "execution");
        assert_eq!(request.facet, "skill_failure");
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
        assert_eq!(request.topic, "execution");
        assert_eq!(request.facet, "errors");
        assert_eq!(request.depth, "diagnostic");
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
        assert_eq!(request.topic, "execution");
        assert_eq!(request.facet, "errors");
        assert_eq!(request.analysis_view, "execution_errors");
    }

    #[test]
    fn decision_trace_uses_trace_analysis_view() {
        let request = ReflectRequest::decision_trace(10, "why this decision?");
        assert_eq!(request.topic, "execution");
        assert_eq!(request.facet, "trace");
        assert_eq!(request.analysis_view, "execution_trace");
        assert_eq!(request.depth, "diagnostic");
        assert_eq!(request.horizon, "session");
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
