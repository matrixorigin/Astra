//! Shared read-only observation DTOs.
//!
//! These are wire-shape types for tool views such as `introspect` and
//! `reflect`. They deliberately do not imply graph persistence or tuning
//! actions; write-side systems may consume these records later.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Canonical observation facet vocabulary shared by `introspect` and `reflect`.
///
/// This enum defines the unified observation-plane taxonomy. Both tools map their
/// legacy facet names to these canonical variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ObservationFacet {
    /// Session-level health: pressure, cache, turns, alerts.
    #[default]
    Session,
    /// Recent execution rounds and tool calls.
    Recent,
    /// Currently-pending volatile injections.
    Volatile,
    /// Stall / loop-guard state and circuit breaker telemetry.
    Stall,
    /// Per-channel freshness of runtime-injected signals.
    Noise,
    /// Recent tool errors and failure previews.
    Errors,
    /// Tool execution trace and outcome history.
    Trace,
    /// Overview: aggregated session summary.
    Overview,
    /// CLI/Edge-local prompt-cache captures and cache diagnosis.
    Cache,
    /// CLI/Edge-local session-memory extraction and injection artifacts.
    SessionMemory,
}

/// Error type for facet parsing failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationFacetError {
    unknown: String,
}

impl fmt::Display for ObservationFacetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown observation facet: {}", self.unknown)
    }
}

impl std::error::Error for ObservationFacetError {}

impl std::str::FromStr for ObservationFacet {
    type Err = ObservationFacetError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "" | "session" | "runtime" => Ok(Self::Session),
            "recent" => Ok(Self::Recent),
            "volatile" => Ok(Self::Volatile),
            "stall" => Ok(Self::Stall),
            "noise" | "freshness" => Ok(Self::Noise),
            "errors" => Ok(Self::Errors),
            "trace" | "history" => Ok(Self::Trace),
            "overview" | "all" => Ok(Self::Overview),
            "cache" => Ok(Self::Cache),
            "session_memory" => Ok(Self::SessionMemory),
            unknown => Err(ObservationFacetError {
                unknown: unknown.to_string(),
            }),
        }
    }
}

impl ObservationFacet {
    /// Convert to canonical string representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Recent => "recent",
            Self::Volatile => "volatile",
            Self::Stall => "stall",
            Self::Noise => "noise",
            Self::Errors => "errors",
            Self::Trace => "trace",
            Self::Overview => "overview",
            Self::Cache => "cache",
            Self::SessionMemory => "session_memory",
        }
    }
}

impl fmt::Display for ObservationFacet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ── Shared observation-plane types ──
// Used by both introspect and reflect for typed request/response fields.

/// Normalized top-level observation topic for both `introspect` and `reflect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationTopic {
    Overview,
    Runtime,
    Execution,
    Knowledge,
}

impl ObservationTopic {
    pub fn from_arg(arg: &str) -> Self {
        match normalize_observation_arg(arg).as_str() {
            "overview" | "" => Self::Overview,
            "execution" => Self::Execution,
            "knowledge" => Self::Knowledge,
            "runtime" | "session" => Self::Runtime,
            _ => Self::Runtime,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::Runtime => "runtime",
            Self::Execution => "execution",
            Self::Knowledge => "knowledge",
        }
    }
}

impl Default for ObservationTopic {
    fn default() -> Self {
        Self::Runtime
    }
}

impl fmt::Display for ObservationTopic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Output depth for observation requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationDepth {
    Hint,
    Summary,
    Diagnostic,
    Forensic,
}

impl ObservationDepth {
    pub fn from_arg(arg: &str) -> Self {
        match normalize_observation_arg(arg).as_str() {
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
}

impl Default for ObservationDepth {
    fn default() -> Self {
        Self::Summary
    }
}

/// Time horizon for the observation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationHorizon {
    Now,
    CurrentTurn,
    Recent,
    Turn,
    Session,
    CrossSession,
}

impl ObservationHorizon {
    pub fn from_arg(arg: &str) -> Self {
        match normalize_observation_arg(arg).as_str() {
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

impl Default for ObservationHorizon {
    fn default() -> Self {
        Self::CurrentTurn
    }
}

/// Data source policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourcePolicy {
    Auto,
    LiveOnly,
    LiveFirst,
    DurableFirst,
    LocalOnly,
    CloudOnly,
}

impl SourcePolicy {
    pub fn from_arg(arg: &str) -> Self {
        match normalize_observation_arg(arg).as_str() {
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

impl Default for SourcePolicy {
    fn default() -> Self {
        Self::Auto
    }
}

/// Shared observation-argument normalizer.
pub fn normalize_observation_arg(arg: &str) -> String {
    arg.trim().to_ascii_lowercase().replace('-', "_")
}

/// Sanitize a string for use in a URN path component.
pub fn urn_component(value: &str) -> String {
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

/// Builder for `urn:astra:{kind}:{namespace}:{component}[:{segments}...]` references.
///
/// Every segment (including `kind`, `namespace`, `component`) is sanitized via
/// [`urn_component`]; indices appended via [`Urn::idx`] are decimal integers and
/// are **not** sanitized.
///
/// # Examples
///
/// ```
/// use astra_core::Urn;
/// let urn = Urn::new("observation", "local", "introspect")
///     .seg("execution")
///     .seg("error")
///     .idx(3)
///     .build();
/// assert_eq!(urn, "urn:astra:observation:local:introspect:execution:error:3");
/// ```
#[derive(Debug, Clone)]
pub struct Urn {
    base: String,
}

impl Urn {
    /// Start a URN with `urn:astra:{kind}:{namespace}:{component}`.
    pub fn new(kind: &str, namespace: &str, component: &str) -> Self {
        Self {
            base: format!(
                "urn:astra:{}:{}:{}",
                urn_component(kind),
                urn_component(namespace),
                urn_component(component),
            ),
        }
    }

    /// Append a sanitized segment.
    pub fn seg(mut self, value: &str) -> Self {
        self.base.push(':');
        self.base.push_str(&urn_component(value));
        self
    }

    /// Append a decimal-index segment (unsanitized).
    pub fn idx(mut self, idx: usize) -> Self {
        use std::fmt::Write;
        let _ = write!(self.base, ":{idx}");
        self
    }

    /// Consume the builder and return the URN string.
    pub fn build(self) -> String {
        self.base
    }
}

const ALLOWED_EVIDENCE_KINDS: &[&str] = &[
    "event",
    "decision",
    "trace",
    "observation",
    "signal",
    "memory",
    "artifact",
    "evaluation",
    "intervention",
    "spec",
    "job",
    "failure_cluster",
    "hypothesis",
    "candidate",
    "condition",
    "reconcile",
    "measurement",
    "context",
];

const ALLOWED_EVIDENCE_NAMESPACES: &[&str] =
    &["cloud", "edge", "local", "graph", "memory", "external"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRef<'a> {
    raw: &'a str,
    kind: &'a str,
    namespace: &'a str,
    id: &'a str,
}

impl<'a> EvidenceRef<'a> {
    pub fn parse(raw: &'a str) -> Result<Self, EvidenceRefError> {
        let mut parts = raw.splitn(5, ':');
        let Some("urn") = parts.next() else {
            return Err(EvidenceRefError::InvalidPrefix);
        };
        let Some("astra") = parts.next() else {
            return Err(EvidenceRefError::InvalidPrefix);
        };
        let kind = parts.next().ok_or(EvidenceRefError::MissingKind)?;
        let namespace = parts.next().ok_or(EvidenceRefError::MissingNamespace)?;
        let id = parts.next().ok_or(EvidenceRefError::MissingId)?;
        if !ALLOWED_EVIDENCE_KINDS.contains(&kind) {
            return Err(EvidenceRefError::UnknownKind(kind.to_string()));
        }
        if !ALLOWED_EVIDENCE_NAMESPACES.contains(&namespace) {
            return Err(EvidenceRefError::UnknownNamespace(namespace.to_string()));
        }
        if id.trim().is_empty()
            || id.chars().any(char::is_whitespace)
            || id.split(':').any(str::is_empty)
        {
            return Err(EvidenceRefError::InvalidId);
        }
        Ok(Self {
            raw,
            kind,
            namespace,
            id,
        })
    }

    pub fn as_str(&self) -> &'a str {
        self.raw
    }

    pub fn kind(&self) -> &'a str {
        self.kind
    }

    pub fn namespace(&self) -> &'a str {
        self.namespace
    }

    pub fn id(&self) -> &'a str {
        self.id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceRefError {
    InvalidPrefix,
    MissingKind,
    MissingNamespace,
    MissingId,
    UnknownKind(String),
    UnknownNamespace(String),
    InvalidId,
}

impl fmt::Display for EvidenceRefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPrefix => write!(f, "evidence ref must start with urn:astra"),
            Self::MissingKind => write!(f, "evidence ref is missing kind"),
            Self::MissingNamespace => write!(f, "evidence ref is missing namespace"),
            Self::MissingId => write!(f, "evidence ref is missing id"),
            Self::UnknownKind(kind) => write!(f, "unknown evidence ref kind: {kind}"),
            Self::UnknownNamespace(namespace) => {
                write!(f, "unknown evidence ref namespace: {namespace}")
            }
            Self::InvalidId => write!(f, "evidence ref id must be non-empty and whitespace-free"),
        }
    }
}

impl std::error::Error for EvidenceRefError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObservationView {
    pub topic: String,
    pub facet: String,
    pub depth: String,
    pub horizon: String,
    pub data_coverage: ObservationDataCoverage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObservationDataCoverage {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub overall: String,
    pub source: String,
    pub events: i64,
    pub decisions: i64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ObservationProviderCoverage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObservationProviderCoverage {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freshness_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ObservationBudgetResult {
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(default, skip_serializing_if = "ObservationBudgetOmitted::is_empty")]
    pub omitted: ObservationBudgetOmitted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ObservationBudgetOmitted {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub nodes: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub evidence_previews: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub observations: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub action_hints: i64,
}

impl ObservationBudgetOmitted {
    pub fn is_empty(&self) -> bool {
        self.nodes == 0
            && self.evidence_previews == 0
            && self.observations == 0
            && self.action_hints == 0
    }
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservationRecord {
    pub ref_id: String,
    pub topic: String,
    pub facet: String,
    pub kind: String,
    pub severity: String,
    pub summary: String,
    pub confidence: ObservationConfidence,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservationConfidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causal: Option<f64>,
}

impl ObservationConfidence {
    pub fn complete(classification: f64, evidence: f64, causal: f64) -> Self {
        Self {
            classification: Some(clamp_confidence(classification)),
            evidence: Some(clamp_confidence(evidence)),
            causal: Some(clamp_confidence(causal)),
        }
    }

    pub fn evidence(evidence: f64) -> Self {
        Self {
            classification: None,
            evidence: Some(clamp_confidence(evidence)),
            causal: None,
        }
    }

    pub fn classification_evidence(classification: f64, evidence: f64) -> Self {
        Self {
            classification: Some(clamp_confidence(classification)),
            evidence: Some(clamp_confidence(evidence)),
            causal: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservationEvidence {
    pub ref_id: String,
    pub evidence_class: String,
    pub source: String,
    pub summary: String,
    pub confidence: ObservationConfidence,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservationActionHint {
    pub target_type: String,
    pub summary: String,
    pub confidence: ObservationConfidence,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observation_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservationFailureCluster {
    pub cluster_ref: String,
    pub label: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observation_refs: Vec<String>,
    pub evidence_class: String,
    pub confidence: ObservationConfidence,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ObservationGraphLayer {
    Runtime,
    Observation,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ObservationGraphNodeKind {
    Event,
    Decision,
    Outcome,
    Observation,
    FailureCluster,
    Evidence,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ObservationGraphEdgeKind {
    Causes,
    Supports,
    Contradicts,
    DerivedFrom,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservationGraphNode {
    pub ref_id: String,
    pub layer: ObservationGraphLayer,
    pub kind: ObservationGraphNodeKind,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObservationGraphEdge {
    pub from: String,
    pub to: String,
    pub kind: ObservationGraphEdgeKind,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ObservationGraphSlice {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<ObservationGraphNode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<ObservationGraphEdge>,
    #[serde(default)]
    pub budget_result: ObservationBudgetResult,
}

// ── Shared graph builders ─────────────────────────────────────────────────

/// Deduplicated node insertion by `ref_id`. Returns `true` if the node was
/// added (i.e. the `ref_id` was not already present).
pub fn push_graph_node(
    nodes: &mut Vec<ObservationGraphNode>,
    node_refs: &mut BTreeSet<String>,
    node: ObservationGraphNode,
) -> bool {
    if node_refs.insert(node.ref_id.clone()) {
        nodes.push(node);
        true
    } else {
        false
    }
}

/// Deduplicated edge insertion by `(from, to, kind)`. Returns `true` if the
/// edge was added.
pub fn push_graph_edge(
    edges: &mut Vec<ObservationGraphEdge>,
    edge_keys: &mut BTreeSet<(String, String, ObservationGraphEdgeKind)>,
    from: String,
    to: String,
    kind: ObservationGraphEdgeKind,
) -> bool {
    if edge_keys.insert((from.clone(), to.clone(), kind)) {
        edges.push(ObservationGraphEdge { from, to, kind });
        true
    } else {
        false
    }
}

/// Trim and truncate a label or summary for graph node display.
/// Returns `None` when the value is empty after trimming.
pub fn truncate_graph_summary(value: &str, max_chars: usize) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let snippet: String = trimmed.chars().take(max_chars).collect();
    Some(snippet)
}

/// Classify an event-type label into an [`ObservationGraphNodeKind`].
/// Error-like and failure-like events map to `Outcome`; everything
/// else maps to `Event`.
pub fn classify_event_kind(event_type: &str) -> ObservationGraphNodeKind {
    if matches!(
        event_type,
        "tool_result" | "tool_error" | "error" | "stall_detected"
    ) || event_type.contains("error")
        || event_type.contains("fail")
    {
        ObservationGraphNodeKind::Outcome
    } else {
        ObservationGraphNodeKind::Event
    }
}

fn clamp_confidence(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observation_facet_parses_advertised_edge_local_facets() {
        assert_eq!(
            "cache".parse::<ObservationFacet>().unwrap(),
            ObservationFacet::Cache
        );
        assert_eq!(
            "session-memory".parse::<ObservationFacet>().unwrap(),
            ObservationFacet::SessionMemory
        );
        assert_eq!(ObservationFacet::Cache.as_str(), "cache");
        assert_eq!(ObservationFacet::SessionMemory.as_str(), "session_memory");
    }

    #[test]
    fn observation_record_wire_shape_roundtrips() {
        let record = ObservationRecord {
            ref_id: "urn:astra:observation:graph:test:1".into(),
            topic: "execution".into(),
            facet: "errors".into(),
            kind: "diagnosis:tool_timeout".into(),
            severity: "warning".into(),
            summary: "tool timed out".into(),
            confidence: ObservationConfidence::complete(0.9, 0.8, 0.7),
            evidence_refs: vec!["urn:astra:event:local:test:1".into()],
        };

        let json = serde_json::to_string(&record).expect("serialize observation");
        let parsed: ObservationRecord =
            serde_json::from_str(&json).expect("deserialize observation");
        assert_eq!(record, parsed);
    }

    #[test]
    fn confidence_wire_shape_names_available_dimensions() {
        let evidence = ObservationEvidence {
            ref_id: "urn:astra:event:local:test:1".into(),
            evidence_class: "observed_evidence".into(),
            source: "test".into(),
            summary: "test evidence".into(),
            confidence: ObservationConfidence::evidence(1.5),
        };
        let json = serde_json::to_value(&evidence).expect("serialize evidence");
        assert_eq!(json["confidence"]["evidence"], 1.0);
        assert!(json["confidence"].get("classification").is_none());
        assert!(json["confidence"].get("causal").is_none());

        let hint = ObservationActionHint {
            target_type: "user_guidance".into(),
            summary: "inspect the failure".into(),
            confidence: ObservationConfidence::classification_evidence(0.8, f64::NAN),
            observation_refs: vec![],
        };
        let json = serde_json::to_value(&hint).expect("serialize action hint");
        assert_eq!(json["confidence"]["classification"], 0.8);
        assert_eq!(json["confidence"]["evidence"], 0.0);
        assert!(json["confidence"].get("causal").is_none());
    }

    #[test]
    fn failure_cluster_wire_shape_is_ref_based() {
        let cluster = ObservationFailureCluster {
            cluster_ref: "urn:astra:failure_cluster:graph:reflect:test:tool_timeout".into(),
            label: "tool_timeout".into(),
            summary: "bash timed out repeatedly".into(),
            observation_refs: vec!["urn:astra:observation:graph:reflect:test:diagnosis:0".into()],
            evidence_class: "inferred_evidence".into(),
            confidence: ObservationConfidence::classification_evidence(0.82, 0.76),
        };

        EvidenceRef::parse(&cluster.cluster_ref).expect("cluster ref must be canonical");
        let cluster_json = serde_json::to_value(&cluster).expect("serialize cluster");
        assert_eq!(cluster_json["confidence"]["classification"], 0.82);
        assert!(cluster_json["confidence"].get("causal").is_none());
    }

    #[test]
    fn graph_slice_wire_shape_keeps_layers_and_edges_explicit() {
        let slice = ObservationGraphSlice {
            nodes: vec![
                ObservationGraphNode {
                    ref_id: "urn:astra:event:cloud:evt-1".into(),
                    layer: ObservationGraphLayer::Runtime,
                    kind: ObservationGraphNodeKind::Event,
                    label: "tool_error".into(),
                    summary: Some("bash timed out".into()),
                    metadata: None,
                },
                ObservationGraphNode {
                    ref_id: "urn:astra:observation:graph:obs-1".into(),
                    layer: ObservationGraphLayer::Observation,
                    kind: ObservationGraphNodeKind::Observation,
                    label: "diagnosis:tool_timeout".into(),
                    summary: None,
                    metadata: None,
                },
            ],
            edges: vec![ObservationGraphEdge {
                from: "urn:astra:observation:graph:obs-1".into(),
                to: "urn:astra:event:cloud:evt-1".into(),
                kind: ObservationGraphEdgeKind::DerivedFrom,
            }],
            budget_result: ObservationBudgetResult::default(),
        };

        for node in &slice.nodes {
            EvidenceRef::parse(&node.ref_id).expect("graph node refs must be canonical");
        }
        let json = serde_json::to_value(&slice).expect("serialize graph slice");
        assert_eq!(json["nodes"][0]["layer"], "runtime");
        assert_eq!(json["nodes"][1]["kind"], "observation");
        assert_eq!(json["edges"][0]["kind"], "derived_from");
    }

    #[test]
    fn evidence_ref_parser_accepts_canonical_refs() {
        let parsed = EvidenceRef::parse("urn:astra:event:cloud:event_01H00000000000000000000001")
            .expect("valid evidence ref");
        assert_eq!(parsed.kind(), "event");
        assert_eq!(parsed.namespace(), "cloud");
        assert_eq!(parsed.id(), "event_01H00000000000000000000001");
        assert_eq!(
            parsed.as_str(),
            "urn:astra:event:cloud:event_01H00000000000000000000001"
        );
    }

    #[test]
    fn evidence_ref_parser_accepts_hierarchical_ids() {
        let edge_event = EvidenceRef::parse("urn:astra:event:edge:session_abc:seq_42")
            .expect("edge event evidence ref should allow hierarchical ids");
        assert_eq!(edge_event.id(), "session_abc:seq_42");

        let reflect_observation =
            EvidenceRef::parse("urn:astra:observation:graph:reflect:test-sess:diagnosis:0")
                .expect("reflect observation ref should allow generated hierarchical ids");
        assert_eq!(reflect_observation.kind(), "observation");
        assert_eq!(reflect_observation.namespace(), "graph");
        assert_eq!(reflect_observation.id(), "reflect:test-sess:diagnosis:0");
    }

    #[test]
    fn evidence_ref_parser_rejects_private_namespaces() {
        let err = EvidenceRef::parse("urn:astra:event:server:evt-1").unwrap_err();
        assert_eq!(err, EvidenceRefError::UnknownNamespace("server".into()));
    }

    #[test]
    fn evidence_ref_parser_rejects_malformed_refs() {
        assert_eq!(
            EvidenceRef::parse("event:cloud:evt-1").unwrap_err(),
            EvidenceRefError::InvalidPrefix
        );
        assert_eq!(
            EvidenceRef::parse("urn:astra").unwrap_err(),
            EvidenceRefError::MissingKind
        );
        assert_eq!(
            EvidenceRef::parse("urn:astra:event").unwrap_err(),
            EvidenceRefError::MissingNamespace
        );
        assert_eq!(
            EvidenceRef::parse("urn:astra:event:cloud").unwrap_err(),
            EvidenceRefError::MissingId
        );
        assert_eq!(
            EvidenceRef::parse("urn:astra:unknown:cloud:evt-1").unwrap_err(),
            EvidenceRefError::UnknownKind("unknown".into())
        );
        assert_eq!(
            EvidenceRef::parse("urn:astra:event:cloud:").unwrap_err(),
            EvidenceRefError::InvalidId
        );
        assert_eq!(
            EvidenceRef::parse("urn:astra:event:cloud:event 1").unwrap_err(),
            EvidenceRefError::InvalidId
        );
        assert_eq!(
            EvidenceRef::parse("urn:astra:event:cloud::evt").unwrap_err(),
            EvidenceRefError::InvalidId
        );
        assert_eq!(
            EvidenceRef::parse("urn:astra:event:cloud:evt:").unwrap_err(),
            EvidenceRefError::InvalidId
        );
        assert_eq!(
            EvidenceRef::parse("urn:astra:event:cloud:evt::seq").unwrap_err(),
            EvidenceRefError::InvalidId
        );
    }

    #[test]
    fn observation_view_can_report_provider_coverage_and_budget() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "live_runtime".to_string(),
            ObservationProviderCoverage {
                status: "fresh".to_string(),
                freshness_ms: Some(0),
                reason: None,
            },
        );
        let coverage = ObservationDataCoverage {
            overall: "partial".to_string(),
            source: "live_runtime_snapshot".to_string(),
            events: 2,
            decisions: 0,
            providers,
            warnings: vec!["memory_backend_unavailable".to_string()],
        };
        let budget = ObservationBudgetResult {
            truncated: true,
            next_cursor: None,
            omitted: ObservationBudgetOmitted {
                nodes: 12,
                evidence_previews: 12,
                ..Default::default()
            },
        };

        let coverage_json = serde_json::to_value(&coverage).expect("serialize coverage");
        assert_eq!(
            coverage_json["providers"]["live_runtime"]["status"],
            "fresh"
        );
        let budget_json = serde_json::to_value(&budget).expect("serialize budget");
        assert_eq!(budget_json["truncated"], true);
        assert_eq!(budget_json["omitted"]["nodes"], 12);
    }

    // ── graph builder tests ──

    #[test]
    fn push_graph_node_inserts_unique_ref_ids() {
        let mut nodes = Vec::new();
        let mut node_refs = BTreeSet::new();

        let added = push_graph_node(
            &mut nodes,
            &mut node_refs,
            ObservationGraphNode {
                ref_id: "urn:astra:event:cloud:evt-1".into(),
                layer: ObservationGraphLayer::Runtime,
                kind: ObservationGraphNodeKind::Event,
                label: "test".into(),
                summary: None,
                metadata: None,
            },
        );
        assert!(added);
        assert_eq!(nodes.len(), 1);
        assert_eq!(node_refs.len(), 1);
    }

    #[test]
    fn push_graph_node_dedup_rejects_duplicate_ref_ids() {
        let mut nodes = Vec::new();
        let mut node_refs = BTreeSet::new();

        let node = ObservationGraphNode {
            ref_id: "urn:astra:event:cloud:evt-1".into(),
            layer: ObservationGraphLayer::Runtime,
            kind: ObservationGraphNodeKind::Event,
            label: "test".into(),
            summary: None,
            metadata: None,
        };
        assert!(push_graph_node(&mut nodes, &mut node_refs, node.clone()));
        assert!(!push_graph_node(&mut nodes, &mut node_refs, node.clone()));
        assert_eq!(nodes.len(), 1);
    }

    #[test]
    fn push_graph_edge_inserts_unique_triples() {
        let mut edges = Vec::new();
        let mut edge_keys = BTreeSet::new();

        let added = push_graph_edge(
            &mut edges,
            &mut edge_keys,
            "from".into(),
            "to".into(),
            ObservationGraphEdgeKind::Causes,
        );
        assert!(added);
        assert_eq!(edges.len(), 1);
    }

    #[test]
    fn push_graph_edge_dedup_rejects_duplicate_triples() {
        let mut edges = Vec::new();
        let mut edge_keys = BTreeSet::new();

        assert!(push_graph_edge(
            &mut edges,
            &mut edge_keys,
            "from".into(),
            "to".into(),
            ObservationGraphEdgeKind::Causes,
        ));
        assert!(!push_graph_edge(
            &mut edges,
            &mut edge_keys,
            "from".into(),
            "to".into(),
            ObservationGraphEdgeKind::Causes,
        ));
        // Different kind = different triple → allowed
        assert!(push_graph_edge(
            &mut edges,
            &mut edge_keys,
            "from".into(),
            "to".into(),
            ObservationGraphEdgeKind::Supports,
        ));
        assert_eq!(edges.len(), 2);
    }

    #[test]
    fn truncate_graph_summary_trims_and_truncates() {
        assert_eq!(
            truncate_graph_summary("  hello world  ", 5),
            Some("hello".into())
        );
        assert_eq!(truncate_graph_summary("   ", 10), None);
        assert_eq!(truncate_graph_summary("", 10), None);
        assert_eq!(truncate_graph_summary("hi", 100), Some("hi".into()));
    }

    #[test]
    fn classify_event_kind_maps_errors_to_outcome() {
        assert_eq!(
            classify_event_kind("tool_error"),
            ObservationGraphNodeKind::Outcome
        );
        assert_eq!(
            classify_event_kind("tool_result"),
            ObservationGraphNodeKind::Outcome
        );
        assert_eq!(
            classify_event_kind("stall_detected"),
            ObservationGraphNodeKind::Outcome
        );
        assert_eq!(
            classify_event_kind("error"),
            ObservationGraphNodeKind::Outcome
        );
        assert_eq!(
            classify_event_kind("bash_failed"),
            ObservationGraphNodeKind::Outcome
        );
    }

    #[test]
    fn classify_event_kind_maps_normal_events_to_event() {
        assert_eq!(
            classify_event_kind("tool_call"),
            ObservationGraphNodeKind::Event
        );
        assert_eq!(
            classify_event_kind("user_message"),
            ObservationGraphNodeKind::Event
        );
        assert_eq!(
            classify_event_kind("skill_started"),
            ObservationGraphNodeKind::Event
        );
    }
}
