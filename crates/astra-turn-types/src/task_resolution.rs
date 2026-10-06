//! References to the existing execution authorities used by journals and verification.

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
