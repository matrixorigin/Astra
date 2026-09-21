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
/// Context metrics use JSON numbers and must remain exactly representable by
/// common consumers such as JavaScript.
pub const EXPLAIN_ANALYZE_MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExplainAnalyzeNodeKindV1 {
    Run,
    Turn,
    Admission,
    Preparation,
    ContextAssembly,
    Judgment,
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

/// Execution boundaries that the current producer cannot independently time.
/// These are attached to a terminal turn fact so consumers never present the
/// observed graph as a complete account of execution.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ExplainAnalyzeCoverageGapV1 {
    ApprovalWaitIntervals,
    UserInputWaitIntervals,
    ProviderRetryBackoff,
    FirstTokenLatency,
    ChildRunIntervals,
    ToolIoWaitIntervals,
}

impl ExplainAnalyzeCoverageGapV1 {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ApprovalWaitIntervals => "approval_wait_intervals",
            Self::UserInputWaitIntervals => "user_input_wait_intervals",
            Self::ProviderRetryBackoff => "provider_retry_backoff",
            Self::FirstTokenLatency => "first_token_latency",
            Self::ChildRunIntervals => "child_run_intervals",
            Self::ToolIoWaitIntervals => "tool_io_wait_intervals",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::ApprovalWaitIntervals => "some approval waits",
            Self::UserInputWaitIntervals => "user input waits",
            Self::ProviderRetryBackoff => "provider retry backoff",
            Self::FirstTokenLatency => "time to first token",
            Self::ChildRunIntervals => "child-run timing",
            Self::ToolIoWaitIntervals => "tool I/O wait breakdown",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExplainAnalyzeUsageBasisV1 {
    ProviderExact,
    ProviderPartial,
    RuntimeEstimated,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExplainAnalyzeContextBudgetBasisV1 {
    PreProviderEstimate,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ExplainAnalyzeContextSourceKindV1 {
    Identity,
    SelfModel,
    ProjectContext,
    DeferredTools,
    AvailableSkills,
    Memory,
    WorkingMemory,
    History,
    Constraints,
    Skills,
    RuntimeIdentity,
    RuntimeVolatile,
    EmergentSkills,
    EmergentMemory,
    EmergentSummary,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExplainAnalyzeContextAssemblyBasisV1 {
    RuntimeTextEstimate,
}

/// Pre-provider estimate for one concrete request attempt. These values
/// describe request construction and are not provider-billed usage.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExplainAnalyzeContextBudgetV1 {
    pub basis: ExplainAnalyzeContextBudgetBasisV1,
    pub estimated_input_tokens: u64,
    pub estimated_system_tokens: u64,
    pub tool_schema_tokens: u64,
    pub requested_output_tokens: u64,
    pub reserved_protocol_tokens: u64,
    pub effective_input_limit_tokens: u64,
    pub model_context_limit_tokens: u64,
    pub visible_tool_count: u32,
}

impl ExplainAnalyzeContextBudgetV1 {
    pub fn is_valid(&self) -> bool {
        [
            self.estimated_input_tokens,
            self.estimated_system_tokens,
            self.tool_schema_tokens,
            self.requested_output_tokens,
            self.reserved_protocol_tokens,
            self.effective_input_limit_tokens,
            self.model_context_limit_tokens,
        ]
        .into_iter()
        .all(|value| value <= EXPLAIN_ANALYZE_MAX_SAFE_INTEGER)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExplainAnalyzeContextSourceV1 {
    pub kind: ExplainAnalyzeContextSourceKindV1,
    pub section_count: u32,
    pub estimated_tokens: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExplainAnalyzeContextAssemblyV1 {
    pub basis: ExplainAnalyzeContextAssemblyBasisV1,
    pub sources: Vec<ExplainAnalyzeContextSourceV1>,
    /// Selected CLI/Edge reports selection, not proof of final prompt injection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edge_memory_selection: Vec<crate::MemorySelectionReport>,
}

impl ExplainAnalyzeContextAssemblyV1 {
    pub fn is_valid(&self) -> bool {
        let mut kinds = HashSet::with_capacity(self.sources.len());
        self.edge_memory_selection.len() <= 2
            && self
                .edge_memory_selection
                .iter()
                .all(crate::MemorySelectionReport::is_valid)
            && self.sources.len() <= 15
            && self.sources.iter().all(|source| {
                source.estimated_tokens <= EXPLAIN_ANALYZE_MAX_SAFE_INTEGER
                    && kinds.insert(source.kind)
            })
    }
}

/// Bounded source costs and a final request estimate have different bases and
/// scopes. Consumers must not sum them together or treat either as billed
/// token usage.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExplainAnalyzeContextMetricsV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<ExplainAnalyzeContextBudgetV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assembly: Option<Box<ExplainAnalyzeContextAssemblyV1>>,
}

impl ExplainAnalyzeContextMetricsV1 {
    pub fn is_valid(&self) -> bool {
        match (&self.budget, &self.assembly) {
            (Some(budget), None) => budget.is_valid(),
            (None, Some(assembly)) => assembly.is_valid(),
            _ => false,
        }
    }
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

/// Read-only physical-attempt usage snapshot for auxiliary inference in one turn.
/// Timing and semantic settlement facts live in the separate
/// `auxiliary_details` field on the terminal turn fact so physical usage is
/// never confused with logical call latency or executor authority.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExplainAnalyzeAuxiliaryUsageV1 {
    pub available: bool,
    /// The bounded capture omitted physical attempts. Counts and token sums
    /// describe captured evidence only, not complete turn/session totals.
    #[serde(default, skip_serializing_if = "auxiliary_capture_not_truncated")]
    pub truncated: bool,
    pub attempts: Vec<ExplainAnalyzeAuxiliaryAttemptV1>,
}

/// One locally measured logical auxiliary call. This interval covers the
/// `SummaryLlmClient` call as observed by the runtime. It is not provider
/// compute time and must not be added to the parent turn interval or to
/// another call's interval.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExplainAnalyzeAuxiliaryCallV1 {
    pub call_id: String,
    pub operation_id: String,
    pub stage: String,
    pub start_elapsed_ms: u64,
    pub duration_ms: u64,
    pub outcome: ExplainAnalyzeOutcomeV1,
}

/// The settlement status at the Work-admission boundary.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExplainAnalyzeAdmissionSettlementStatusV1 {
    Accepted,
    Rejected,
    Unavailable,
    NotDispatched,
}

/// Typed reason for the admission settlement. Provider response text is not
/// part of the Explain contract.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExplainAnalyzeAdmissionSettlementReasonV1 {
    Accepted,
    ClassifierUncertain,
    ClassifierConflicting,
    InvalidClassifierResponse,
    ProviderRejected,
    PlanningRejected,
    ReconciliationRejected,
    Unavailable {
        reason: crate::SemanticJudgmentUnavailableReasonV1,
    },
    NotDispatched {
        reason: crate::SemanticJudgmentPreDispatchReasonV1,
    },
}

/// Bounded classifier evidence and the runtime's admission settlement. The
/// classification is deliberately separate from settlement: a valid
/// classification can still be rejected later by planning or reconciliation.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExplainAnalyzeAdmissionSettlementV1 {
    pub status: ExplainAnalyzeAdmissionSettlementStatusV1,
    pub reason: ExplainAnalyzeAdmissionSettlementReasonV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<crate::RequestJudgmentResultV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<crate::RequestJudgmentResultV1>,
}

/// Bounded semantic and timing details attached to the terminal turn fact.
/// These details are observational and never grant execution authority.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExplainAnalyzeAuxiliaryDetailsV1 {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<ExplainAnalyzeAuxiliaryCallV1>,
    #[serde(default, skip_serializing_if = "auxiliary_details_not_truncated")]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<ExplainAnalyzeAdmissionSettlementV1>,
}

fn auxiliary_details_not_truncated(value: &bool) -> bool {
    !value
}

impl ExplainAnalyzeAuxiliaryDetailsV1 {
    const MAX_CALLS: usize = 16;

    pub fn is_valid(&self, terminal_elapsed_ms: u64) -> bool {
        let mut call_ids = HashSet::new();
        self.calls.len() <= Self::MAX_CALLS
            && self.calls.iter().all(|call| {
                valid_id(&call.call_id)
                    && valid_id(&call.operation_id)
                    && valid_id(&call.stage)
                    && call.start_elapsed_ms <= terminal_elapsed_ms
                    && call
                        .start_elapsed_ms
                        .checked_add(call.duration_ms)
                        .is_some_and(|end| end <= terminal_elapsed_ms.saturating_add(1))
                    && call_ids.insert(&call.call_id)
            }) && self.admission.as_ref().is_none_or(|admission| {
            admission
                .classification
                .as_ref()
                .is_none_or(|classification| classification.validate().is_ok())
                && admission
                    .decision
                    .as_ref()
                    .is_none_or(|decision| decision.validate().is_ok())
                && match (&admission.status, &admission.reason) {
                    (
                        ExplainAnalyzeAdmissionSettlementStatusV1::Accepted,
                        ExplainAnalyzeAdmissionSettlementReasonV1::Accepted,
                    ) => admission.decision.as_ref().is_some_and(|decision| {
                        matches!(decision, crate::RequestJudgmentResultV1::Decided { .. })
                    }),
                    (ExplainAnalyzeAdmissionSettlementStatusV1::Rejected, reason) => {
                        !matches!(
                            reason,
                            ExplainAnalyzeAdmissionSettlementReasonV1::Accepted
                                | ExplainAnalyzeAdmissionSettlementReasonV1::Unavailable { .. }
                                | ExplainAnalyzeAdmissionSettlementReasonV1::NotDispatched { .. }
                        ) && admission.decision.is_none()
                    }
                    (
                        ExplainAnalyzeAdmissionSettlementStatusV1::Unavailable,
                        ExplainAnalyzeAdmissionSettlementReasonV1::Unavailable { .. },
                    )
                    | (
                        ExplainAnalyzeAdmissionSettlementStatusV1::NotDispatched,
                        ExplainAnalyzeAdmissionSettlementReasonV1::NotDispatched { .. },
                    ) => admission.decision.is_none(),
                    _ => false,
                }
        })
    }
}

fn auxiliary_capture_not_truncated(value: &bool) -> bool {
    !value
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExplainAnalyzeAuxiliaryAttemptV1 {
    pub attempt_id: String,
    pub usage_status: ExplainAnalyzeAuxiliaryUsageStatusV1,
    pub provider: String,
    pub offering_id: String,
    pub model_name: String,
    pub purpose: String,
    pub operation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ExplainAnalyzeTokenUsageV1>,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExplainAnalyzeAuxiliaryUsageStatusV1 {
    ProviderExact,
    ProviderPartial,
    Unavailable,
}

impl ExplainAnalyzeAuxiliaryUsageV1 {
    pub fn is_valid(&self) -> bool {
        let mut seen = std::collections::HashSet::new();
        (self.available || (self.attempts.is_empty() && !self.truncated))
            && self.attempts.iter().all(|a| {
                valid_id(&a.attempt_id)
                    && seen.insert(&a.attempt_id)
                    && valid_id(&a.provider)
                    && valid_id(&a.offering_id)
                    && valid_id(&a.purpose)
                    && valid_id(&a.operation_id)
                    && match a.usage_status {
                        ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderExact => a
                            .usage
                            .as_ref()
                            .is_some_and(|u| u.basis == ExplainAnalyzeUsageBasisV1::ProviderExact),
                        ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderPartial => a
                            .usage
                            .as_ref()
                            .is_none_or(|u| u.basis == ExplainAnalyzeUsageBasisV1::ProviderPartial),
                        ExplainAnalyzeAuxiliaryUsageStatusV1::Unavailable => a.usage.is_none(),
                    }
                    && !a.model_name.trim().is_empty()
                    && a.model_name.len() <= 255
                    && !a.model_name.chars().any(char::is_control)
                    && a.usage.as_ref().is_none_or(|u| {
                        u.is_valid() && u.basis != ExplainAnalyzeUsageBasisV1::RuntimeEstimated
                    })
            })
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
    /// Auxiliary usage is separate from timed provider-attempt node usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auxiliary_usage: Option<Box<ExplainAnalyzeAuxiliaryUsageV1>>,
    /// Auxiliary timing and accepted semantic settlement are separate from
    /// physical provider-attempt usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auxiliary_details: Option<Box<ExplainAnalyzeAuxiliaryDetailsV1>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ExplainAnalyzeContextMetricsV1>,
    /// Known boundaries without a measured graph interval. Only terminal turn
    /// facts may carry coverage so all renderers share one scope and source.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coverage_gaps: Vec<ExplainAnalyzeCoverageGapV1>,
}

impl ExplainAnalyzeEventV1 {
    /// Validate a decoded/public event before projection or graph mutation.
    pub fn is_valid(&self) -> bool {
        if self.auxiliary_usage.as_ref().is_some_and(|a| {
            self.kind != ExplainAnalyzeNodeKindV1::Turn
                || self.transition != ExplainAnalyzeTransitionV1::Finished
                || !a.is_valid()
        }) {
            return false;
        }
        if self.auxiliary_details.as_ref().is_some_and(|details| {
            self.kind != ExplainAnalyzeNodeKindV1::Turn
                || self.transition != ExplainAnalyzeTransitionV1::Finished
                || !details.is_valid(self.elapsed_ms)
        }) {
            return false;
        }
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
                    && self.auxiliary_details.is_none()
                    && self.context.is_none()
                    && self.coverage_gaps.is_empty()
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
                    && (self.context.is_none() || self.usage.is_none())
                    && self.context.as_ref().is_none_or(|context| {
                        context.is_valid()
                            && match self.kind {
                                ExplainAnalyzeNodeKindV1::Preparation => {
                                    context.budget.is_some() && context.assembly.is_none()
                                }
                                ExplainAnalyzeNodeKindV1::ContextAssembly => {
                                    context.budget.is_none() && context.assembly.is_some()
                                }
                                _ => false,
                            }
                    })
                    && self.coverage_gaps.len() <= 8
                    && (self.coverage_gaps.is_empty()
                        || self.kind == ExplainAnalyzeNodeKindV1::Turn)
                    && self
                        .coverage_gaps
                        .iter()
                        .copied()
                        .collect::<HashSet<_>>()
                        .len()
                        == self.coverage_gaps.len()
                    && self
                        .coverage_gaps
                        .windows(2)
                        .all(|pair| pair[0].as_str() < pair[1].as_str())
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
    use crate::{
        RequestJudgmentCapabilityV1, RequestJudgmentClassificationV1, RequestJudgmentDomainV1,
        RequestJudgmentMutationV1, RequestJudgmentResultV1, RequestJudgmentScopeV1,
        RequestJudgmentStageV1, SEMANTIC_JUDGMENT_PRESENTATION_MAX_BYTES, SemanticJudgmentFactV1,
    };

    #[test]
    fn auxiliary_capture_overflow_is_partial_evidence_not_unavailability() {
        use super::ExplainAnalyzeAuxiliaryUsageV1;
        let legacy = r#"{"available":true,"attempts":[]}"#;
        let mut facts: ExplainAnalyzeAuxiliaryUsageV1 = serde_json::from_str(legacy).unwrap();
        assert!(!facts.truncated);
        assert_eq!(serde_json::to_string(&facts).unwrap(), legacy);
        facts.truncated = true;
        assert!(facts.is_valid());
        let roundtrip: ExplainAnalyzeAuxiliaryUsageV1 =
            serde_json::from_value(serde_json::to_value(&facts).unwrap()).unwrap();
        assert!(roundtrip.truncated);
        facts.available = false;
        assert!(!facts.is_valid());
    }

    use super::*;

    fn started() -> ExplainAnalyzeEventV1 {
        ExplainAnalyzeEventV1 {
            auxiliary_usage: None,
            auxiliary_details: None,
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
            context: None,
            coverage_gaps: Vec::new(),
        }
    }

    #[test]
    fn semantic_judgment_presentation_is_valid_for_explain_event() {
        let fact = SemanticJudgmentFactV1 {
            stage: RequestJudgmentStageV1::Clarification,
            result: RequestJudgmentResultV1::Decided {
                classification: RequestJudgmentClassificationV1 {
                    work_required: true,
                    activation_deferred: true,
                    domain: Some(RequestJudgmentDomainV1::Database),
                    mutation: RequestJudgmentMutationV1::MustMutate,
                    scope: RequestJudgmentScopeV1::Mixed,
                    parallel_subruns: true,
                    capabilities: vec![
                        RequestJudgmentCapabilityV1::Web,
                        RequestJudgmentCapabilityV1::AgentSpawner,
                    ],
                },
            },
        };
        let mut event = started();
        event.label = fact.presentation_label();
        assert!(event.label.len() <= SEMANTIC_JUDGMENT_PRESENTATION_MAX_BYTES);
        assert!(event.is_valid(), "{}", event.label);
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
    fn auxiliary_details_validate_bounds_and_runtime_acceptance() {
        let mut event = terminal(started());
        event.kind = ExplainAnalyzeNodeKindV1::Turn;
        event.node_id = "turn-1".to_string();
        event.parent_node_id = None;
        event.auxiliary_details = Some(Box::new(ExplainAnalyzeAuxiliaryDetailsV1 {
            calls: vec![ExplainAnalyzeAuxiliaryCallV1 {
                call_id: "request_judgment:initial:0".into(),
                operation_id: "request_judgment".into(),
                stage: "initial".into(),
                start_elapsed_ms: 15,
                duration_ms: 15,
                outcome: ExplainAnalyzeOutcomeV1::Succeeded,
            }],
            truncated: false,
            admission: Some(ExplainAnalyzeAdmissionSettlementV1 {
                status: ExplainAnalyzeAdmissionSettlementStatusV1::Accepted,
                reason: ExplainAnalyzeAdmissionSettlementReasonV1::Accepted,
                classification: Some(crate::RequestJudgmentResultV1::Decided {
                    classification: crate::RequestJudgmentClassificationV1 {
                        work_required: false,
                        activation_deferred: false,
                        domain: None,
                        mutation: crate::RequestJudgmentMutationV1::ReadOnly,
                        scope: crate::RequestJudgmentScopeV1::Unknown,
                        parallel_subruns: false,
                        capabilities: Vec::new(),
                    },
                }),
                decision: Some(crate::RequestJudgmentResultV1::Decided {
                    classification: crate::RequestJudgmentClassificationV1 {
                        work_required: false,
                        activation_deferred: false,
                        domain: None,
                        mutation: crate::RequestJudgmentMutationV1::ReadOnly,
                        scope: crate::RequestJudgmentScopeV1::Unknown,
                        parallel_subruns: false,
                        capabilities: Vec::new(),
                    },
                }),
            }),
        }));
        assert!(event.is_valid());

        let mut out_of_bounds = event.clone();
        out_of_bounds.auxiliary_details.as_mut().unwrap().calls[0].start_elapsed_ms = 31;
        assert!(!out_of_bounds.is_valid());

        let mut not_decided = event;
        not_decided
            .auxiliary_details
            .as_mut()
            .unwrap()
            .admission
            .as_mut()
            .unwrap()
            .status = ExplainAnalyzeAdmissionSettlementStatusV1::Accepted;
        not_decided
            .auxiliary_details
            .as_mut()
            .unwrap()
            .admission
            .as_mut()
            .unwrap()
            .reason = ExplainAnalyzeAdmissionSettlementReasonV1::Accepted;
        not_decided
            .auxiliary_details
            .as_mut()
            .unwrap()
            .admission
            .as_mut()
            .unwrap()
            .classification = Some(crate::RequestJudgmentResultV1::Unavailable {
            reason: crate::SemanticJudgmentUnavailableReasonV1::Deadline,
            delivery: crate::SemanticJudgmentDeliveryV1::Unresolved,
        });
        not_decided
            .auxiliary_details
            .as_mut()
            .unwrap()
            .admission
            .as_mut()
            .unwrap()
            .decision = None;
        assert!(!not_decided.is_valid());
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
    fn coverage_gaps_are_unique_terminal_turn_facts_only() {
        let mut turn = started();
        turn.kind = ExplainAnalyzeNodeKindV1::Turn;
        turn.node_id = "turn-1".to_string();
        turn.parent_node_id = None;
        turn.round_index = None;
        turn.attempt_index = None;
        turn = terminal(turn);
        turn.coverage_gaps = vec![
            ExplainAnalyzeCoverageGapV1::ChildRunIntervals,
            ExplainAnalyzeCoverageGapV1::ToolIoWaitIntervals,
        ];
        assert!(turn.is_valid());

        let mut duplicate = turn.clone();
        duplicate
            .coverage_gaps
            .push(ExplainAnalyzeCoverageGapV1::ChildRunIntervals);
        assert!(!duplicate.is_valid());

        let mut non_turn = terminal(started());
        non_turn.coverage_gaps = vec![ExplainAnalyzeCoverageGapV1::ChildRunIntervals];
        assert!(!non_turn.is_valid());

        let mut started_with_coverage = started();
        started_with_coverage.coverage_gaps = vec![ExplainAnalyzeCoverageGapV1::ChildRunIntervals];
        assert!(!started_with_coverage.is_valid());
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

    fn terminal(mut event: ExplainAnalyzeEventV1) -> ExplainAnalyzeEventV1 {
        event.transition = ExplainAnalyzeTransitionV1::Finished;
        event.elapsed_ms = 30;
        event.start_elapsed_ms = Some(15);
        event.duration_ms = Some(15);
        event.outcome = Some(ExplainAnalyzeOutcomeV1::Succeeded);
        event
    }

    fn context_budget() -> ExplainAnalyzeContextBudgetV1 {
        ExplainAnalyzeContextBudgetV1 {
            basis: ExplainAnalyzeContextBudgetBasisV1::PreProviderEstimate,
            estimated_input_tokens: 100,
            estimated_system_tokens: 30,
            tool_schema_tokens: 20,
            requested_output_tokens: 40,
            reserved_protocol_tokens: 10,
            effective_input_limit_tokens: 800,
            model_context_limit_tokens: 1_000,
            visible_tool_count: 2,
        }
    }

    fn context_assembly() -> ExplainAnalyzeContextAssemblyV1 {
        ExplainAnalyzeContextAssemblyV1 {
            edge_memory_selection: Vec::new(),
            basis: ExplainAnalyzeContextAssemblyBasisV1::RuntimeTextEstimate,
            sources: vec![ExplainAnalyzeContextSourceV1 {
                kind: ExplainAnalyzeContextSourceKindV1::Identity,
                section_count: 2,
                estimated_tokens: 30,
            }],
        }
    }

    #[test]
    fn context_facts_are_terminal_kind_scoped_and_reject_empty_or_mixed_payloads() {
        let mut preparation = terminal(started());
        preparation.kind = ExplainAnalyzeNodeKindV1::Preparation;
        preparation.context = Some(ExplainAnalyzeContextMetricsV1 {
            budget: Some(context_budget()),
            assembly: None,
        });
        assert!(preparation.is_valid());

        let mut assembly = terminal(started());
        assembly.kind = ExplainAnalyzeNodeKindV1::ContextAssembly;
        assembly.context = Some(ExplainAnalyzeContextMetricsV1 {
            budget: None,
            assembly: Some(Box::new(context_assembly())),
        });
        assert!(assembly.is_valid());

        let mut started_with_context = started();
        started_with_context.context = preparation.context.clone();
        assert!(!started_with_context.is_valid());

        let mut wrong_kind = terminal(started());
        wrong_kind.context = preparation.context.clone();
        assert!(!wrong_kind.is_valid());

        preparation.context = Some(ExplainAnalyzeContextMetricsV1 {
            budget: None,
            assembly: None,
        });
        assert!(!preparation.is_valid());

        assembly.context = Some(ExplainAnalyzeContextMetricsV1 {
            budget: Some(context_budget()),
            assembly: Some(Box::new(context_assembly())),
        });
        assert!(!assembly.is_valid());

        let mut mixed = terminal(started());
        mixed.kind = ExplainAnalyzeNodeKindV1::Preparation;
        mixed.context = Some(ExplainAnalyzeContextMetricsV1 {
            budget: Some(context_budget()),
            assembly: None,
        });
        mixed.usage = Some(ExplainAnalyzeTokenUsageV1 {
            basis: ExplainAnalyzeUsageBasisV1::ProviderExact,
            fresh_input_tokens: Some(100),
            cache_read_tokens: None,
            cache_creation_tokens: None,
            output_tokens: Some(12),
        });
        assert!(!mixed.is_valid());
    }

    #[test]
    fn context_metrics_reject_duplicate_sources_unknown_fields_and_unsafe_numbers() {
        let mut duplicate_source = context_assembly();
        duplicate_source
            .sources
            .push(duplicate_source.sources[0].clone());
        assert!(!duplicate_source.is_valid());

        let mut unsafe_budget = context_budget();
        unsafe_budget.estimated_input_tokens = EXPLAIN_ANALYZE_MAX_SAFE_INTEGER + 1;
        assert!(!unsafe_budget.is_valid());

        let mut unsafe_source = context_assembly();
        unsafe_source.sources[0].estimated_tokens = EXPLAIN_ANALYZE_MAX_SAFE_INTEGER + 1;
        assert!(!unsafe_source.is_valid());

        let mut budget_json = serde_json::to_value(context_budget()).unwrap();
        budget_json["prompt_text"] = serde_json::json!("private prompt must not enter facts");
        assert!(serde_json::from_value::<ExplainAnalyzeContextBudgetV1>(budget_json).is_err());

        let mut assembly_json = serde_json::to_value(context_assembly()).unwrap();
        assembly_json["private_memory"] = serde_json::json!("private memory must not enter facts");
        assert!(serde_json::from_value::<ExplainAnalyzeContextAssemblyV1>(assembly_json).is_err());

        let mut unknown_source = serde_json::to_value(context_assembly()).unwrap();
        unknown_source["sources"][0]["kind"] = serde_json::json!("future_private_source");
        assert!(serde_json::from_value::<ExplainAnalyzeContextAssemblyV1>(unknown_source).is_err());
    }

    #[test]
    fn context_facts_round_trip_without_source_content() {
        let mut event = terminal(started());
        event.kind = ExplainAnalyzeNodeKindV1::ContextAssembly;
        event.context = Some(ExplainAnalyzeContextMetricsV1 {
            budget: None,
            assembly: Some(Box::new(context_assembly())),
        });

        let encoded = serde_json::to_string(&event).unwrap();
        assert!(encoded.contains("runtime_text_estimate"));
        assert!(encoded.contains("estimated_tokens"));
        assert!(!encoded.contains("prompt"));
        assert!(event.is_valid());
        assert_eq!(
            serde_json::from_str::<ExplainAnalyzeEventV1>(&encoded).unwrap(),
            event
        );
    }
}
