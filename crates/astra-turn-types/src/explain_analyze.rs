//! Versioned public facts used to build a live and replayable execution graph.
//!
//! This schema carries bounded lifecycle facts only. Prompts, chain-of-thought,
//! credentials, tool arguments, and tool output belong to other explicitly
//! authorized surfaces and are not fields of this protocol.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

pub const EXPLAIN_ANALYZE_SCHEMA_VERSION: u16 = 1;
pub const EXPLAIN_ANALYZE_EVENT_TYPE: &str = "explain_analyze";
const EXPLAIN_ID_MAX_BYTES: usize = 512;
const EXPLAIN_LABEL_MAX_BYTES: usize = 160;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExplainAnalyzeNodeKindV1 {
    Run,
    Turn,
    Admission,
    Preparation,
    ContextAssembly,
    ModelRound,
    ProviderAttempt,
    ToolBatch,
    ToolCall,
    Wait,
    ChildRun,
    Settlement,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExplainAnalyzeTransitionV1 {
    Started,
    Finished,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExplainAnalyzeOutcomeV1 {
    Completed,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
    Blocked,
    Waiting,
    Rejected,
    Reused,
    Suppressed,
    Deferred,
    Resolved,
    Fallback,
    Unavailable,
    Delegated,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExplainAnalyzeUsageBasisV1 {
    ProviderExact,
    ProviderPartial,
    RuntimeEstimated,
}

/// Provider-reported or estimated token lanes for one physical provider
/// attempt. `None` means unavailable, not zero. Cache lanes retain the source
/// provider's overlap semantics; consumers must not assume the lanes are
/// additive or disjoint.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExplainAnalyzeTokenUsageV1 {
    pub basis: ExplainAnalyzeUsageBasisV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fresh_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
}

impl ExplainAnalyzeTokenUsageV1 {
    pub fn is_valid(&self) -> bool {
        self.fresh_input_tokens.is_some()
            || self.cache_read_tokens.is_some()
            || self.cache_creation_tokens.is_some()
            || self.output_tokens.is_some()
    }
}

/// One idempotent fact about a node in a run/turn execution graph.
///
/// `elapsed_ms` is measured in `clock_domain_id` from the producer's turn
/// origin. A finish event repeats the original start offset and carries the
/// measured duration, so a consumer can reconstruct the node even if the
/// start event was missed. Offsets from different clock domains are never
/// comparable without a separate alignment fact.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExplainAnalyzeEventV1 {
    pub schema_version: u16,
    pub event_id: String,
    pub run_id: String,
    pub turn_id: String,
    pub node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependency_node_ids: Vec<String>,
    pub producer_id: String,
    pub clock_domain_id: String,
    pub kind: ExplainAnalyzeNodeKindV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_index: Option<u32>,
    /// A bounded runtime-authored label, never user or provider payload text.
    pub label: String,
    pub transition: ExplainAnalyzeTransitionV1,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_elapsed_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ExplainAnalyzeOutcomeV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ExplainAnalyzeTokenUsageV1>,
}

impl ExplainAnalyzeEventV1 {
    /// Validate a decoded/public event before projection or graph mutation.
    pub fn is_valid(&self) -> bool {
        if self.schema_version != EXPLAIN_ANALYZE_SCHEMA_VERSION
            || !valid_id(&self.event_id)
            || !valid_id(&self.run_id)
            || !valid_id(&self.turn_id)
            || !valid_id(&self.node_id)
            || !valid_id(&self.producer_id)
            || !valid_id(&self.clock_domain_id)
            || self.label.trim().is_empty()
            || self.label.len() > EXPLAIN_LABEL_MAX_BYTES
        {
            return false;
        }

        if self
            .parent_node_id
            .as_deref()
            .is_some_and(|parent| !valid_id(parent) || parent == self.node_id)
        {
            return false;
        }

        if (self.kind == ExplainAnalyzeNodeKindV1::ModelRound && self.round_index.is_none())
            || (self.kind == ExplainAnalyzeNodeKindV1::ProviderAttempt
                && (self.round_index.is_none() || self.attempt_index.is_none()))
        {
            return false;
        }

        let mut dependencies = HashSet::with_capacity(self.dependency_node_ids.len());
        if self.dependency_node_ids.iter().any(|dependency| {
            !valid_id(dependency) || dependency == &self.node_id || !dependencies.insert(dependency)
        }) {
            return false;
        }

        match self.transition {
            ExplainAnalyzeTransitionV1::Started => {
                self.start_elapsed_ms.is_none()
                    && self.duration_ms.is_none()
                    && self.outcome.is_none()
                    && self.usage.is_none()
            }
            ExplainAnalyzeTransitionV1::Finished => {
                self.start_elapsed_ms.is_some_and(|start| {
                    start <= self.elapsed_ms
                        && self.duration_ms.is_some_and(|duration| {
                            self.elapsed_ms.abs_diff(start).abs_diff(duration) <= 1
                        })
                }) && self.outcome.is_some()
                    && self
                        .usage
                        .as_ref()
                        .is_none_or(ExplainAnalyzeTokenUsageV1::is_valid)
            }
        }
    }

    pub fn event_type(&self) -> &'static str {
        EXPLAIN_ANALYZE_EVENT_TYPE
    }
}

fn valid_id(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= EXPLAIN_ID_MAX_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started() -> ExplainAnalyzeEventV1 {
        ExplainAnalyzeEventV1 {
            schema_version: EXPLAIN_ANALYZE_SCHEMA_VERSION,
            event_id: "turn-1/provider/0/started".to_string(),
            run_id: "run-1".to_string(),
            turn_id: "turn-1".to_string(),
            node_id: "turn-1/provider/0".to_string(),
            parent_node_id: Some("turn-1".to_string()),
            dependency_node_ids: vec![],
            producer_id: "runtime-worker-1".to_string(),
            clock_domain_id: "worker-1/turn-1".to_string(),
            kind: ExplainAnalyzeNodeKindV1::ProviderAttempt,
            round_index: Some(0),
            attempt_index: Some(0),
            label: "Model request".to_string(),
            transition: ExplainAnalyzeTransitionV1::Started,
            elapsed_ms: 15,
            start_elapsed_ms: None,
            duration_ms: None,
            outcome: None,
            usage: None,
        }
    }

    #[test]
    fn explain_analyze_event_is_closed_versioned_and_round_trips() {
        let event = started();
        assert!(event.is_valid());
        let encoded = serde_json::to_value(&event).unwrap();
        assert!(encoded.get("type").is_none());
        let decoded: ExplainAnalyzeEventV1 = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, event);

        let mut with_unknown = serde_json::to_value(event).unwrap();
        with_unknown["private_prompt"] = serde_json::json!("must not enter this protocol");
        assert!(serde_json::from_value::<ExplainAnalyzeEventV1>(with_unknown).is_err());
    }

    #[test]
    fn terminal_event_carries_reconstructable_interval_and_usage() {
        let mut event = started();
        event.event_id = "turn-1/provider/0/finished".to_string();
        event.transition = ExplainAnalyzeTransitionV1::Finished;
        event.elapsed_ms = 89;
        event.start_elapsed_ms = Some(15);
        event.duration_ms = Some(73);
        event.outcome = Some(ExplainAnalyzeOutcomeV1::Succeeded);
        event.usage = Some(ExplainAnalyzeTokenUsageV1 {
            basis: ExplainAnalyzeUsageBasisV1::ProviderExact,
            fresh_input_tokens: Some(310),
            cache_read_tokens: Some(120),
            cache_creation_tokens: None,
            output_tokens: Some(44),
        });

        assert!(event.is_valid());
        assert_eq!(event.event_type(), "explain_analyze");
        assert_eq!(event.elapsed_ms - event.start_elapsed_ms.unwrap(), 74);
        assert_eq!(event.duration_ms, Some(73));
    }

    #[test]
    fn provider_attempt_identity_and_measured_interval_are_validated() {
        let mut terminal = started();
        terminal.transition = ExplainAnalyzeTransitionV1::Finished;
        terminal.elapsed_ms = 101;
        terminal.start_elapsed_ms = Some(15);
        terminal.duration_ms = Some(86);
        terminal.outcome = Some(ExplainAnalyzeOutcomeV1::Succeeded);
        assert!(terminal.is_valid());

        terminal.duration_ms = Some(20);
        assert!(
            !terminal.is_valid(),
            "a graph interval must agree with its measured duration within timestamp rounding"
        );

        terminal.duration_ms = Some(86);
        terminal.attempt_index = None;
        assert!(
            !terminal.is_valid(),
            "provider retries need a typed physical attempt identity"
        );
    }

    #[test]
    fn invalid_ids_edges_transitions_and_empty_usage_are_rejected() {
        let mut event = started();
        event.parent_node_id = Some(event.node_id.clone());
        assert!(!event.is_valid());

        let mut event = started();
        event.dependency_node_ids = vec!["prior".to_string(), "prior".to_string()];
        assert!(!event.is_valid());

        let mut event = started();
        event.transition = ExplainAnalyzeTransitionV1::Finished;
        event.start_elapsed_ms = Some(event.elapsed_ms + 1);
        event.duration_ms = Some(1);
        event.outcome = Some(ExplainAnalyzeOutcomeV1::Failed);
        assert!(!event.is_valid());

        let mut event = started();
        event.transition = ExplainAnalyzeTransitionV1::Finished;
        event.start_elapsed_ms = Some(0);
        event.duration_ms = Some(15);
        event.outcome = Some(ExplainAnalyzeOutcomeV1::Succeeded);
        event.usage = Some(ExplainAnalyzeTokenUsageV1 {
            basis: ExplainAnalyzeUsageBasisV1::RuntimeEstimated,
            fresh_input_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            output_tokens: None,
        });
        assert!(!event.is_valid());
    }

    #[test]
    fn unsupported_versions_and_impossible_start_payloads_are_rejected() {
        let mut event = started();
        event.schema_version = EXPLAIN_ANALYZE_SCHEMA_VERSION + 1;
        assert!(!event.is_valid());

        let mut event = started();
        event.outcome = Some(ExplainAnalyzeOutcomeV1::Succeeded);
        assert!(!event.is_valid());
    }
}
