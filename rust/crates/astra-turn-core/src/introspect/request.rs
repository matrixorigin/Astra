use astra_core::{
    ObservationDepth, ObservationFacet, ObservationHorizon, ObservationTopic, SourcePolicy,
};
use serde_json::Value;
use std::str::FromStr;

// Compatibility aliases for existing callers.
pub use astra_core::ObservationDepth as IntrospectDepth;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntrospectFormat {
    Text,
    Json,
}

impl IntrospectFormat {
    fn from_arg(arg: &str) -> Self {
        match astra_core::normalize_observation_arg(arg).as_str() {
            "json" | "structured" | "envelope" => Self::Json,
            _ => Self::Text,
        }
    }

    pub fn is_json(self) -> bool {
        self == Self::Json
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntrospectRequest {
    pub topic: ObservationTopic,
    pub facet: ObservationFacet,
    pub depth: ObservationDepth,
    pub horizon: ObservationHorizon,
    pub source_policy: SourcePolicy,
    pub include_context: bool,
    pub format: IntrospectFormat,
}

impl Default for IntrospectRequest {
    fn default() -> Self {
        Self {
            topic: ObservationTopic::Runtime,
            facet: ObservationFacet::Session,
            depth: ObservationDepth::Summary,
            horizon: ObservationHorizon::CurrentTurn,
            source_policy: SourcePolicy::Auto,
            include_context: false,
            format: IntrospectFormat::Text,
        }
    }
}

impl IntrospectRequest {
    pub fn from_args(args: &Value) -> Self {
        let topic_arg = string_arg(args, "topic");
        let topic_raw = topic_arg.unwrap_or("runtime");
        let (topic_head, topic_facet) = split_topic_facet(topic_raw);
        let mut topic = ObservationTopic::from_arg(topic_head);

        let facet_candidate = string_arg(args, "facet")
            .or(topic_facet)
            .unwrap_or_else(|| default_facet_for_topic(topic));
        let (facet_topic, facet_tail) = split_topic_facet(facet_candidate);
        if topic_arg.is_none() && facet_tail.is_some() {
            topic = ObservationTopic::from_arg(facet_topic);
        }
        let facet_raw = facet_tail.unwrap_or(facet_candidate);

        let depth_raw = string_arg(args, "depth").unwrap_or("summary");

        let facet = match ObservationFacet::from_str(facet_raw) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("Invalid facet '{}': {}, using default", facet_raw, e);
                ObservationFacet::default()
            }
        };

        Self {
            topic,
            facet,
            depth: ObservationDepth::from_arg(depth_raw),
            horizon: ObservationHorizon::from_arg(
                string_arg(args, "horizon").unwrap_or("current_turn"),
            ),
            source_policy: SourcePolicy::from_arg(
                string_arg(args, "source_policy").unwrap_or("auto"),
            ),
            include_context: bool_arg(args, "include_context"),
            format: IntrospectFormat::from_arg(string_arg(args, "format").unwrap_or("text")),
        }
    }
}

fn string_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).map(str::trim)
}

fn bool_arg(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn split_topic_facet(topic: &str) -> (&str, Option<&str>) {
    topic
        .split_once('/')
        .map(|(head, tail)| (head.trim(), Some(tail.trim())))
        .unwrap_or((topic.trim(), None))
}

fn default_facet_for_topic(topic: ObservationTopic) -> &'static str {
    match topic {
        ObservationTopic::Execution => "errors",
        ObservationTopic::Knowledge => "session_memory",
        _ => "session",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_args_ignores_removed_legacy_subtopic_and_detail() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "subtopic": "tool_errors",
            "detail": "full"
        }));
        assert_eq!(req.topic, ObservationTopic::Runtime);
        assert_eq!(req.facet, ObservationFacet::Session);
        assert_eq!(req.depth, ObservationDepth::Summary);
        assert_eq!(req.horizon, ObservationHorizon::CurrentTurn);
    }

    #[test]
    fn from_args_supports_topic_facet_depth_horizon() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "topic": "execution/errors",
            "depth": "forensic",
            "horizon": "session",
            "source_policy": "cloud_only",
            "include_context": true
        }));
        assert_eq!(req.topic, ObservationTopic::Execution);
        assert_eq!(req.facet, ObservationFacet::Errors);
        assert_eq!(req.depth, ObservationDepth::Forensic);
        assert_eq!(req.horizon, ObservationHorizon::Session);
        assert_eq!(req.source_policy, SourcePolicy::CloudOnly);
        assert!(req.include_context);
    }

    #[test]
    fn from_args_uses_hint_as_canonical_depth() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "depth": "hint"
        }));
        assert_eq!(req.depth, ObservationDepth::Hint);
        assert_eq!(req.depth.as_str(), "hint");
    }

    #[test]
    fn from_args_does_not_accept_removed_minimal_depth_alias() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "depth": "minimal"
        }));
        assert_eq!(req.depth, ObservationDepth::Summary);
        assert_eq!(req.depth.as_str(), "summary");
    }

    #[test]
    fn from_args_infers_topic_from_slash_in_facet_arg() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "facet": "execution/trace"
        }));
        assert_eq!(req.topic, ObservationTopic::Execution);
        assert_eq!(req.facet, ObservationFacet::Trace);
    }

    #[test]
    fn from_args_topic_path_normalizes_to_topic_facet() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "topic": "knowledge/session_memory"
        }));
        assert_eq!(req.topic, ObservationTopic::Knowledge);
        assert_eq!(req.facet, ObservationFacet::SessionMemory);
    }

    #[test]
    fn from_args_parses_advertised_cache_facet() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "facet": "cache"
        }));
        assert_eq!(req.facet, ObservationFacet::Cache);
    }

    #[test]
    fn from_args_parses_json_format() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "format": "json"
        }));
        assert!(req.format.is_json());
    }

    #[test]
    fn from_args_does_not_accept_premature_adaptation_topic() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "topic": "adaptation"
        }));
        assert_eq!(req.topic, ObservationTopic::Runtime);
        assert_eq!(req.topic.as_str(), "runtime");
    }
}
