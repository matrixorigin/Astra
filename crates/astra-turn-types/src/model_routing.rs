//! Versioned model-selection facts. Offerings remain the admission authority.
use serde::{Deserialize, Serialize};

pub const DETERMINISTIC_ROUTING_POLICY_VERSION: &str = "easy-read-only-v1";
pub const MODEL_ROUTING_FEATURE_VERSION: u8 = 1;

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

/// Frozen before primary inference. Outcomes and follow-up sentiment never enter
/// this allowlisted feature schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRoutingFeatures {
    pub schema_version: u8,
    pub assessment_present: bool,
    pub difficulty: crate::TaskDifficulty,
    pub difficulty_confidence: crate::AssessmentConfidence,
    pub read_only_primary: bool,
    pub supported_input: bool,
}

impl ModelRoutingFeatures {
    pub fn new(
        assessment: Option<crate::TurnAssessment>,
        read_only_primary: bool,
        supported_input: bool,
    ) -> Self {
        let observed = assessment.unwrap_or_default();
        Self {
            schema_version: MODEL_ROUTING_FEATURE_VERSION,
            assessment_present: assessment.is_some(),
            difficulty: observed.difficulty,
            difficulty_confidence: observed.difficulty_confidence,
            read_only_primary,
            supported_input,
        }
    }
}
