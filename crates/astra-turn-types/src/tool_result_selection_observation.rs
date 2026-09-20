//! Bounded, content-free observations for optional tool-result selection.
//!
//! These facts explain an evaluation. They are not provider-wire receipts and
//! never establish that a recommendation was included in a model request.

use serde::{Deserialize, Serialize};

use crate::{ToolResultProjectionDispositionV1, ToolResultProjectionFallbackV1};

pub const TOOL_RESULT_SELECTION_OBSERVATION_SCHEMA_VERSION: u16 = 1;
pub const TOOL_RESULT_SELECTION_TRACE_ATTR: &str = "tool_result_selection.v1";
pub const TOOL_RESULT_SELECTION_OBSERVATION_MAX_BYTES: usize = 8_192;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResultSelectionCorrelationV1 {
    pub run_id: String,
    pub turn: u32,
    pub round: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_generation: Option<u64>,
    pub evaluation_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResultSelectionCoverageV1 {
    pub source_bytes: u64,
    pub scanned_bytes: u64,
    pub candidate_chunks: u32,
    pub source_complete: bool,
    pub goal_complete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultSelectionNotDispatchedReasonV1 {
    NoOffering,
    InvalidRequest,
    OutputBudget,
    RouteUnavailable,
    DurableMaterialUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultSelectionUnavailableReasonV1 {
    Cancelled,
    ExecutionError,
    ProviderPtlError,
    UnexpectedFinish,
    InvalidResponse,
    MissingExecutionProvenance,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolResultSelectionOutcomeV1 {
    Started,
    Decided {
        decision_sha256: String,
        disposition: ToolResultProjectionDispositionV1,
        selected_chunks: u32,
        relevant_chunks: u32,
        uncertain_chunks: u32,
        irrelevant_chunks: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fallback: Option<ToolResultProjectionFallbackV1>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        judgment_invocation_id: Option<String>,
    },
    Baseline {
        decision_sha256: String,
        fallback: ToolResultProjectionFallbackV1,
    },
    NotDispatched {
        reason: ToolResultSelectionNotDispatchedReasonV1,
    },
    Unavailable {
        reason: ToolResultSelectionUnavailableReasonV1,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        judgment_invocation_id: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResultSelectionObservationV1 {
    pub schema_version: u16,
    pub correlation: ToolResultSelectionCorrelationV1,
    pub coverage: ToolResultSelectionCoverageV1,
    pub outcome: ToolResultSelectionOutcomeV1,
}

impl ToolResultSelectionObservationV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != TOOL_RESULT_SELECTION_OBSERVATION_SCHEMA_VERSION
            || !valid_id(&self.correlation.run_id, 64)
            || !valid_id(&self.correlation.evaluation_id, 512)
            || self.coverage.source_bytes == 0
            || self.coverage.scanned_bytes > self.coverage.source_bytes
            || self.coverage.source_complete
                != (self.coverage.scanned_bytes == self.coverage.source_bytes)
            || self.coverage.candidate_chunks == 0
            || self.coverage.candidate_chunks > 32
        {
            return Err("invalid tool-result selection observation");
        }
        match &self.outcome {
            ToolResultSelectionOutcomeV1::Started => {
                if !self.coverage.source_complete || !self.coverage.goal_complete {
                    return Err("tool-result selection cannot start with incomplete evidence");
                }
                Ok(())
            }
            ToolResultSelectionOutcomeV1::Decided {
                decision_sha256,
                disposition,
                selected_chunks,
                relevant_chunks,
                uncertain_chunks,
                irrelevant_chunks,
                fallback,
                judgment_invocation_id,
            } => {
                if relevant_chunks
                    .saturating_add(*uncertain_chunks)
                    .saturating_add(*irrelevant_chunks)
                    != self.coverage.candidate_chunks
                {
                    return Err("tool-result selection counts do not cover candidates");
                }
                if !self.coverage.source_complete
                    || !self.coverage.goal_complete
                    || judgment_invocation_id.is_none()
                    || *selected_chunks > self.coverage.candidate_chunks
                    || match (disposition, fallback) {
                        (ToolResultProjectionDispositionV1::Selected, None) => {
                            *relevant_chunks == 0
                                || *selected_chunks
                                    != relevant_chunks.saturating_add(*uncertain_chunks)
                        }
                        (
                            ToolResultProjectionDispositionV1::Baseline,
                            Some(ToolResultProjectionFallbackV1::NoClearMatch),
                        ) => *relevant_chunks != 0 || *selected_chunks != 0,
                        (
                            ToolResultProjectionDispositionV1::Baseline,
                            Some(ToolResultProjectionFallbackV1::ProjectionNotSmaller),
                        ) => *relevant_chunks == 0 || *selected_chunks != 0,
                        _ => true,
                    }
                {
                    return Err("inconsistent model-derived selection decision");
                }
                validate_decision(
                    decision_sha256,
                    *disposition,
                    *selected_chunks,
                    *fallback,
                    judgment_invocation_id.as_deref(),
                )
            }
            ToolResultSelectionOutcomeV1::NotDispatched { .. } => Ok(()),
            ToolResultSelectionOutcomeV1::Baseline {
                decision_sha256,
                fallback,
            } => {
                if !is_sha256(decision_sha256)
                    || *fallback != ToolResultProjectionFallbackV1::IncompleteCoverage
                    || (self.coverage.source_complete && self.coverage.goal_complete)
                {
                    return Err("invalid deterministic baseline decision observation");
                }
                Ok(())
            }
            ToolResultSelectionOutcomeV1::Unavailable {
                judgment_invocation_id,
                ..
            } => {
                if judgment_invocation_id
                    .as_deref()
                    .is_some_and(|id| !valid_id(id, 256))
                {
                    return Err("invalid tool-result selection invocation identity");
                }
                Ok(())
            }
        }?;
        let encoded =
            serde_json::to_vec(self).map_err(|_| "serialize tool-result selection observation")?;
        if encoded.len() > TOOL_RESULT_SELECTION_OBSERVATION_MAX_BYTES {
            return Err("tool-result selection observation is too large");
        }
        Ok(())
    }
}

fn validate_decision(
    decision_sha256: &str,
    disposition: ToolResultProjectionDispositionV1,
    selected_chunks: u32,
    fallback: Option<ToolResultProjectionFallbackV1>,
    judgment_invocation_id: Option<&str>,
) -> Result<(), &'static str> {
    let valid_baseline_fallback = matches!(
        fallback,
        Some(
            ToolResultProjectionFallbackV1::NoClearMatch
                | ToolResultProjectionFallbackV1::ProjectionNotSmaller
        )
    );
    if !is_sha256(decision_sha256)
        || selected_chunks > 32
        || judgment_invocation_id.is_some_and(|id| !valid_id(id, 256))
        || matches!(disposition, ToolResultProjectionDispositionV1::Selected)
            != (selected_chunks > 0 && fallback.is_none() && judgment_invocation_id.is_some())
        || matches!(disposition, ToolResultProjectionDispositionV1::Baseline)
            != (selected_chunks == 0 && valid_baseline_fallback)
    {
        return Err("invalid tool-result selection decision observation");
    }
    Ok(())
}

fn valid_id(value: &str, max_bytes: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(outcome: ToolResultSelectionOutcomeV1) -> ToolResultSelectionObservationV1 {
        ToolResultSelectionObservationV1 {
            schema_version: TOOL_RESULT_SELECTION_OBSERVATION_SCHEMA_VERSION,
            correlation: ToolResultSelectionCorrelationV1 {
                run_id: "run-1".into(),
                turn: 1,
                round: 2,
                owner_generation: Some(3),
                evaluation_id: "evaluation-1".into(),
            },
            coverage: ToolResultSelectionCoverageV1 {
                source_bytes: 100,
                scanned_bytes: 100,
                candidate_chunks: 2,
                source_complete: true,
                goal_complete: true,
            },
            outcome,
        }
    }

    #[test]
    fn selected_decision_requires_real_invocation_and_matching_counts() {
        let valid = observation(ToolResultSelectionOutcomeV1::Decided {
            decision_sha256: "a".repeat(64),
            disposition: ToolResultProjectionDispositionV1::Selected,
            selected_chunks: 1,
            relevant_chunks: 1,
            uncertain_chunks: 0,
            irrelevant_chunks: 1,
            fallback: None,
            judgment_invocation_id: Some("invocation-1".into()),
        });
        valid.validate().unwrap();

        let mut invalid = valid;
        if let ToolResultSelectionOutcomeV1::Decided {
            judgment_invocation_id,
            ..
        } = &mut invalid.outcome
        {
            *judgment_invocation_id = None;
        }
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn unavailable_is_not_an_application_claim() {
        observation(ToolResultSelectionOutcomeV1::NotDispatched {
            reason: ToolResultSelectionNotDispatchedReasonV1::NoOffering,
        })
        .validate()
        .unwrap();
        observation(ToolResultSelectionOutcomeV1::Unavailable {
            reason: ToolResultSelectionUnavailableReasonV1::ProviderPtlError,
            judgment_invocation_id: None,
        })
        .validate()
        .unwrap();
    }

    #[test]
    fn baseline_provenance_matches_how_the_decision_was_made() {
        let model_baseline = observation(ToolResultSelectionOutcomeV1::Decided {
            decision_sha256: "b".repeat(64),
            disposition: ToolResultProjectionDispositionV1::Baseline,
            selected_chunks: 0,
            relevant_chunks: 0,
            uncertain_chunks: 2,
            irrelevant_chunks: 0,
            fallback: Some(ToolResultProjectionFallbackV1::NoClearMatch),
            judgment_invocation_id: None,
        });
        assert!(model_baseline.validate().is_err());

        let mut incomplete = observation(ToolResultSelectionOutcomeV1::Baseline {
            decision_sha256: "c".repeat(64),
            fallback: ToolResultProjectionFallbackV1::IncompleteCoverage,
        });
        assert!(incomplete.validate().is_err());
        incomplete.coverage.goal_complete = false;
        incomplete.validate().unwrap();
    }

    #[test]
    fn selected_decision_cannot_exceed_or_bypass_covered_candidates() {
        let mut selected = observation(ToolResultSelectionOutcomeV1::Decided {
            decision_sha256: "d".repeat(64),
            disposition: ToolResultProjectionDispositionV1::Selected,
            selected_chunks: 3,
            relevant_chunks: 1,
            uncertain_chunks: 0,
            irrelevant_chunks: 1,
            fallback: None,
            judgment_invocation_id: Some("invocation-1".into()),
        });
        assert!(selected.validate().is_err());

        selected.coverage.goal_complete = false;
        if let ToolResultSelectionOutcomeV1::Decided {
            selected_chunks, ..
        } = &mut selected.outcome
        {
            *selected_chunks = 1;
        }
        assert!(selected.validate().is_err());
    }

    #[test]
    fn started_observation_requires_complete_evidence() {
        let mut started = observation(ToolResultSelectionOutcomeV1::Started);
        started.validate().unwrap();
        started.coverage.source_complete = false;
        started.coverage.scanned_bytes = 99;
        assert!(started.validate().is_err());
    }
}
