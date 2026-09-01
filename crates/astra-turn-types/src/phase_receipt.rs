//! Versioned, transport-safe receipts for completed turn lifecycle phases.
//!
//! These types live below the server and service crates so live delivery,
//! durable replay, traces, and CLI Explain all validate exactly the same
//! protocol without introducing a dependency cycle.

use serde::{Deserialize, Serialize};

/// Schema version for bounded completed turn-phase receipts.
pub const TURN_PHASE_SCHEMA_VERSION: u16 = 1;
/// Stable event type shared by the live SSE and durable replay projections.
pub const TURN_PHASE_EVENT_TYPE: &str = "turn_phase";

/// A completed phase that materially contributed to a user's observed turn
/// latency. Each enum member corresponds to a runtime-owned boundary.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhaseKindV1 {
    TurnIntentAdmission,
    RequestPreparation,
    ModelInference,
    ToolExecution,
}

/// Non-sensitive result class for a completed phase. Detailed causes remain
/// in correlated trace/log attributes and never cross the public receipt.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhaseOutcomeV1 {
    Decided,
    FixedDefault,
    Delegated,
    Unavailable,
    Succeeded,
    Failed,
}

impl TurnPhaseOutcomeV1 {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Decided => "decided",
            Self::FixedDefault => "fixed_default",
            Self::Delegated => "delegated",
            Self::Unavailable => "unavailable",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

/// Versioned public projection of one completed execution phase. Durations
/// are wall time measured at the phase owner, not a sum of token estimates or
/// concurrent child durations.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TurnPhaseReceiptV1 {
    pub schema_version: u16,
    pub phase: TurnPhaseKindV1,
    /// Zero-based logical LLM round. Semantic admission belongs to round zero
    /// because it gates the first request; this is an ordering key, not a
    /// provider-call count.
    pub round_index: u32,
    /// Zero-based physical attempt within a logical round.
    pub attempt_index: u32,
    pub outcome: TurnPhaseOutcomeV1,
    pub duration_ms: u64,
}

impl TurnPhaseReceiptV1 {
    /// Reject semantically impossible phase/outcome combinations at every
    /// transport boundary. This validates a typed protocol; it does not infer
    /// behavior from unstructured model or user text.
    pub const fn is_valid(&self) -> bool {
        self.schema_version == TURN_PHASE_SCHEMA_VERSION
            && match self.phase {
                TurnPhaseKindV1::TurnIntentAdmission => matches!(
                    self.outcome,
                    TurnPhaseOutcomeV1::Decided
                        | TurnPhaseOutcomeV1::FixedDefault
                        | TurnPhaseOutcomeV1::Delegated
                        | TurnPhaseOutcomeV1::Unavailable
                ),
                TurnPhaseKindV1::RequestPreparation
                | TurnPhaseKindV1::ModelInference
                | TurnPhaseKindV1::ToolExecution => matches!(
                    self.outcome,
                    TurnPhaseOutcomeV1::Succeeded | TurnPhaseOutcomeV1::Failed
                ),
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(phase: TurnPhaseKindV1, outcome: TurnPhaseOutcomeV1) -> TurnPhaseReceiptV1 {
        TurnPhaseReceiptV1 {
            schema_version: TURN_PHASE_SCHEMA_VERSION,
            phase,
            round_index: 0,
            attempt_index: 0,
            outcome,
            duration_ms: 1,
        }
    }

    #[test]
    fn phase_outcome_contract_has_no_cross_phase_fallbacks() {
        for outcome in [
            TurnPhaseOutcomeV1::Decided,
            TurnPhaseOutcomeV1::FixedDefault,
            TurnPhaseOutcomeV1::Delegated,
            TurnPhaseOutcomeV1::Unavailable,
        ] {
            assert!(receipt(TurnPhaseKindV1::TurnIntentAdmission, outcome).is_valid());
        }
        assert!(
            !receipt(
                TurnPhaseKindV1::TurnIntentAdmission,
                TurnPhaseOutcomeV1::Succeeded
            )
            .is_valid()
        );

        for phase in [
            TurnPhaseKindV1::RequestPreparation,
            TurnPhaseKindV1::ModelInference,
            TurnPhaseKindV1::ToolExecution,
        ] {
            assert!(receipt(phase, TurnPhaseOutcomeV1::Succeeded).is_valid());
            assert!(receipt(phase, TurnPhaseOutcomeV1::Failed).is_valid());
            assert!(!receipt(phase, TurnPhaseOutcomeV1::Decided).is_valid());
        }
    }

    #[test]
    fn receipt_schema_is_closed_and_versioned() {
        let parsed = serde_json::from_value::<TurnPhaseReceiptV1>(serde_json::json!({
            "schema_version": TURN_PHASE_SCHEMA_VERSION,
            "phase": "model_inference",
            "round_index": 2,
            "outcome": "succeeded",
            "duration_ms": 17,
            "untrusted_detail": "not part of the protocol",
        }));
        assert!(parsed.is_err());

        let mut stale = receipt(
            TurnPhaseKindV1::ModelInference,
            TurnPhaseOutcomeV1::Succeeded,
        );
        stale.schema_version = TURN_PHASE_SCHEMA_VERSION + 1;
        assert!(!stale.is_valid());
    }

    #[test]
    fn receipt_without_attempt_index_is_rejected() {
        let error = serde_json::from_value::<TurnPhaseReceiptV1>(serde_json::json!({
            "schema_version": TURN_PHASE_SCHEMA_VERSION,
            "phase": "model_inference",
            "round_index": 2,
            "outcome": "succeeded",
            "duration_ms": 17,
        }))
        .expect_err("attempt_index is required by the current receipt protocol");
        assert!(error.to_string().contains("attempt_index"));
    }
}
