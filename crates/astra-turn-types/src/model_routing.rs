//! Versioned model-selection facts. Offerings remain the admission authority.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRoutingMode {
    #[default]
    Explicit,
    Auto,
}

impl ModelRoutingMode {
    pub fn is_explicit(&self) -> bool {
        matches!(self, Self::Explicit)
    }
}

/// Operator-qualified pair. Revision identifies the operator's evaluation,
/// including model revisions and price comparison; names imply no quality tier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoModelRoutingPolicy {
    pub revision: String,
    pub economy_offering_id: String,
    pub strong_offering_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRoutingReason {
    EasyReadOnly,
    AssessmentUnavailable,
    InsufficientConfidence,
    StrongRequired,
    EconomyUnavailable,
    IncompatibleCandidate,
    UnsupportedInput,
}
