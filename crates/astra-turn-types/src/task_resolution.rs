//! Provider-neutral model assessments retained separately from execution facts.

use serde::{Deserialize, Serialize};

/// A reference to an existing execution authority, not another execution ledger.
/// Resolving it must validate the owner, terminal payload and immutable digest
/// against that authority before any task-level interpretation is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "authority",
    content = "completion",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ToolExecutionEvidenceRef {
    Invocation(Box<crate::ToolInvocationCompletionRef>),
    EdgeDispatch(EdgeDispatchCompletionRef),
}

/// Server-owned reference to an accepted durable Edge callback. The normalized
/// invocation ID is the dispatch request ID; it does not create an invocation
/// ledger row. A process-local callback alone cannot mint this reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeDispatchCompletionRef {
    pub identity: crate::ToolInvocationIdentity,
    pub edge_agent_id: String,
    pub result_hash: String,
}

impl ToolExecutionEvidenceRef {
    pub fn identity(&self) -> &crate::ToolInvocationIdentity {
        match self {
            Self::Invocation(reference) => &reference.identity,
            Self::EdgeDispatch(reference) => &reference.identity,
        }
    }

    /// Only invocation-backed evidence may enter existing invocation-only
    /// deterministic verifier contracts. Edge assessment does not widen them.
    pub fn as_invocation(&self) -> Option<&crate::ToolInvocationCompletionRef> {
        match self {
            Self::Invocation(reference) => Some(reference),
            Self::EdgeDispatch(_) => None,
        }
    }
}

impl From<crate::ToolInvocationCompletionRef> for ToolExecutionEvidenceRef {
    fn from(reference: crate::ToolInvocationCompletionRef) -> Self {
        Self::Invocation(Box::new(reference))
    }
}

/// Request-local authority minted only by shared completion-action admission.
/// Deliberately not serializable/deserializable as provider arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskResolutionSubmissionAuthority {
    boundary_id: String,
    logical_call_id: String,
}

impl TaskResolutionSubmissionAuthority {
    pub fn for_admitted_call(boundary_id: &str, logical_call_id: &str) -> Option<Self> {
        if boundary_id.trim().is_empty() || logical_call_id.trim().is_empty() {
            return None;
        }
        Some(Self {
            boundary_id: boundary_id.into(),
            logical_call_id: logical_call_id.into(),
        })
    }

    pub fn boundary_id(&self) -> &str {
        &self.boundary_id
    }

    pub fn for_call(&self, logical_call_id: &str) -> Option<&Self> {
        (self.logical_call_id == logical_call_id).then_some(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskResolutionConclusion {
    Supported,
    Partial,
    Unknown,
}

/// Model-authored interpretation only. Execution scope and reconciliation
/// boundary belong to the runtime and cannot be supplied by the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskResolutionProposal {
    pub verification_target: String,
    pub failed_call_ids: Vec<String>,
    pub evidence_call_ids: Vec<String>,
    pub conclusion: TaskResolutionConclusion,
    pub rationale: String,
    pub remaining_gaps: Vec<String>,
}

/// A model interpretation, never an execution receipt. The runtime checks scope
/// and boundary against its own state. Target and rationale describe the claim;
/// neither grants evidence identity, execution authority or verification success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskResolutionAssessment {
    pub scope: String,
    pub boundary_id: String,
    pub verification_target: String,
    pub failed_call_ids: Vec<String>,
    pub evidence_call_ids: Vec<String>,
    pub conclusion: TaskResolutionConclusion,
    pub rationale: String,
    pub remaining_gaps: Vec<String>,
}
