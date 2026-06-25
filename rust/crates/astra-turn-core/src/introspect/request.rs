use serde_json::Value;

use super::IntrospectTextDepth;

/// Normalized top-level observation topic for the `introspect` tool.
///
/// `introspect` remains a runtime/current-turn tool. These topic names mirror
/// the broader observation-plane vocabulary without making this renderer own
/// the future persistent graph model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationTopic {
    Overview,
    Runtime,
    Execution,
    Knowledge,
    Adaptation,
}

impl ObservationTopic {
    fn from_arg(arg: &str) -> Self {
        match normalize_arg(arg).as_str() {
            "overview" => Self::Overview,
            "execution" => Self::Execution,
            "knowledge" => Self::Knowledge,
            "adaptation" => Self::Adaptation,
            "runtime" | "session" | "" => Self::Runtime,
            _ => Self::Runtime,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::Runtime => "runtime",
            Self::Execution => "execution",
            Self::Knowledge => "knowledge",
            Self::Adaptation => "adaptation",
        }
    }
}

/// Runtime facet selected within an observation topic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntrospectFacet {
    Session,
    Cache,
    Recent,
    Volatile,
    Stall,
    Noise,
    Errors,
    SessionMemory,
    All,
    Unknown(String),
}

impl IntrospectFacet {
    fn from_arg(arg: &str) -> Self {
        match normalize_arg(arg).as_str() {
            "" | "session" | "runtime" | "overview" => Self::Session,
            "cache" | "execution/cache" | "runtime/cache" | "performance/cache" => Self::Cache,
            "recent" | "execution/recent" => Self::Recent,
            "volatile" | "runtime/volatile" => Self::Volatile,
            "stall" | "execution/stall" => Self::Stall,
            "noise" | "knowledge/freshness" => Self::Noise,
            "errors" | "execution/errors" => Self::Errors,
            "session_memory" | "knowledge/memory" => Self::SessionMemory,
            "all" => Self::All,
            other => Self::Unknown(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Session => "session",
            Self::Cache => "cache",
            Self::Recent => "recent",
            Self::Volatile => "volatile",
            Self::Stall => "stall",
            Self::Noise => "noise",
            Self::Errors => "errors",
            Self::SessionMemory => "session_memory",
            Self::All => "all",
            Self::Unknown(value) => value.as_str(),
        }
    }
}

/// Output depth requested by callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntrospectDepth {
    Hint,
    Summary,
    Diagnostic,
    Forensic,
}

impl IntrospectDepth {
    fn from_arg(arg: &str) -> Self {
        match normalize_arg(arg).as_str() {
            "hint" => Self::Hint,
            "diagnostic" => Self::Diagnostic,
            "forensic" => Self::Forensic,
            "summary" | "" => Self::Summary,
            _ => Self::Summary,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hint => "hint",
            Self::Summary => "summary",
            Self::Diagnostic => "diagnostic",
            Self::Forensic => "forensic",
        }
    }

    pub(super) fn detail(self) -> IntrospectTextDepth {
        match self {
            Self::Hint => IntrospectTextDepth::Hint,
            Self::Summary => IntrospectTextDepth::Summary,
            Self::Diagnostic | Self::Forensic => IntrospectTextDepth::Full,
        }
    }
}

/// Time horizon for the observation request. It is parsed and carried for
/// semantic consistency; the current runtime snapshot can only satisfy
/// current-turn/session-local horizons until persistent graph storage exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationHorizon {
    Now,
    CurrentTurn,
    Recent,
    Turn,
    Session,
    CrossSession,
}

impl ObservationHorizon {
    fn from_arg(arg: &str) -> Self {
        match normalize_arg(arg).as_str() {
            "now" => Self::Now,
            "recent" => Self::Recent,
            "turn" => Self::Turn,
            "session" => Self::Session,
            "cross_session" | "cross-session" => Self::CrossSession,
            "current_turn" | "current-turn" | "" => Self::CurrentTurn,
            _ => Self::CurrentTurn,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Now => "now",
            Self::CurrentTurn => "current_turn",
            Self::Recent => "recent",
            Self::Turn => "turn",
            Self::Session => "session",
            Self::CrossSession => "cross_session",
        }
    }
}

/// Data source policy requested by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourcePolicy {
    Auto,
    LiveOnly,
    LiveFirst,
    DurableFirst,
    LocalOnly,
    CloudOnly,
}

impl SourcePolicy {
    fn from_arg(arg: &str) -> Self {
        match normalize_arg(arg).as_str() {
            "live_only" => Self::LiveOnly,
            "live_first" => Self::LiveFirst,
            "durable_first" => Self::DurableFirst,
            "local_only" => Self::LocalOnly,
            "cloud_only" => Self::CloudOnly,
            "auto" | "" => Self::Auto,
            _ => Self::Auto,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::LiveOnly => "live_only",
            Self::LiveFirst => "live_first",
            Self::DurableFirst => "durable_first",
            Self::LocalOnly => "local_only",
            Self::CloudOnly => "cloud_only",
        }
    }

    pub fn allows_edge_local_artifacts(self) -> bool {
        matches!(
            self,
            Self::Auto | Self::LiveFirst | Self::DurableFirst | Self::LocalOnly
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntrospectFormat {
    Text,
    Json,
}

impl IntrospectFormat {
    fn from_arg(arg: &str) -> Self {
        match normalize_arg(arg).as_str() {
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
    pub facet: IntrospectFacet,
    pub depth: IntrospectDepth,
    pub horizon: ObservationHorizon,
    pub source_policy: SourcePolicy,
    pub include_context: bool,
    pub format: IntrospectFormat,
}

impl Default for IntrospectRequest {
    fn default() -> Self {
        Self {
            topic: ObservationTopic::Runtime,
            facet: IntrospectFacet::Session,
            depth: IntrospectDepth::Summary,
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

        Self {
            topic,
            facet: IntrospectFacet::from_arg(facet_raw),
            depth: IntrospectDepth::from_arg(depth_raw),
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

fn normalize_arg(arg: &str) -> String {
    arg.trim().to_ascii_lowercase().replace('-', "_")
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
        assert_eq!(req.facet, IntrospectFacet::Session);
        assert_eq!(req.depth, IntrospectDepth::Summary);
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
        assert_eq!(req.facet, IntrospectFacet::Errors);
        assert_eq!(req.depth, IntrospectDepth::Forensic);
        assert_eq!(req.horizon, ObservationHorizon::Session);
        assert_eq!(req.source_policy, SourcePolicy::CloudOnly);
        assert!(req.include_context);
    }

    #[test]
    fn from_args_uses_hint_as_canonical_depth() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "depth": "hint"
        }));
        assert_eq!(req.depth, IntrospectDepth::Hint);
        assert_eq!(req.depth.as_str(), "hint");
    }

    #[test]
    fn from_args_does_not_accept_removed_minimal_depth_alias() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "depth": "minimal"
        }));
        assert_eq!(req.depth, IntrospectDepth::Summary);
        assert_eq!(req.depth.as_str(), "summary");
    }

    #[test]
    fn from_args_infers_topic_from_slash_facet() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "facet": "execution/errors"
        }));
        assert_eq!(req.topic, ObservationTopic::Execution);
        assert_eq!(req.facet, IntrospectFacet::Errors);
    }

    #[test]
    fn from_args_parses_json_format() {
        let req = IntrospectRequest::from_args(&serde_json::json!({
            "format": "json"
        }));
        assert!(req.format.is_json());
    }
}
