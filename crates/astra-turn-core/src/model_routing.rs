//! Shared model selection and offline evaluation. No credentials, catalog or provider I/O.
use astra_turn_types::model_routing::ModelRoutingReason;
use astra_turn_types::{AssessmentConfidence, TaskDifficulty, TurnAssessment};

pub mod offline;
pub mod qualification;

pub use astra_turn_types::model_routing::DETERMINISTIC_ROUTING_POLICY_VERSION as POLICY_VERSION;

/// Work/capability facts are supplied by the canonical semantic admission.
/// Satisfaction and urgency deliberately do not select a cheaper model.
pub fn economy_eligibility(
    assessment: Option<TurnAssessment>,
    read_only_primary: bool,
    supported_input: bool,
) -> ModelRoutingReason {
    economy_eligibility_from_features(astra_turn_types::model_routing::ModelRoutingFeatures::new(
        assessment,
        read_only_primary,
        supported_input,
    ))
}

pub fn economy_eligibility_from_features(
    features: astra_turn_types::model_routing::ModelRoutingFeatures,
) -> ModelRoutingReason {
    if features.schema_version != astra_turn_types::model_routing::MODEL_ROUTING_FEATURE_VERSION
        || !features.supported_input
    {
        return ModelRoutingReason::UnsupportedInput;
    }
    if !features.assessment_present {
        return ModelRoutingReason::AssessmentUnavailable;
    }
    if features.difficulty_confidence != AssessmentConfidence::High {
        return ModelRoutingReason::InsufficientConfidence;
    }
    if features.difficulty != TaskDifficulty::Easy || !features.read_only_primary {
        return ModelRoutingReason::StrongRequired;
    }
    ModelRoutingReason::EasyReadOnly
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_confident_easy_read_only_tasks_are_eligible() {
        for difficulty in [
            TaskDifficulty::Unknown,
            TaskDifficulty::Easy,
            TaskDifficulty::Moderate,
            TaskDifficulty::Difficult,
        ] {
            for confidence in [
                AssessmentConfidence::Unknown,
                AssessmentConfidence::Low,
                AssessmentConfidence::Medium,
                AssessmentConfidence::High,
            ] {
                for read_only in [false, true] {
                    for supported in [false, true] {
                        let assessment = TurnAssessment {
                            difficulty,
                            difficulty_confidence: confidence,
                            ..Default::default()
                        };
                        assert_eq!(
                            economy_eligibility(Some(assessment), read_only, supported)
                                == ModelRoutingReason::EasyReadOnly,
                            difficulty == TaskDifficulty::Easy
                                && confidence == AssessmentConfidence::High
                                && read_only
                                && supported
                        );
                    }
                }
            }
        }
        assert_eq!(
            economy_eligibility(None, true, true),
            ModelRoutingReason::AssessmentUnavailable
        );
    }

    #[test]
    fn satisfaction_and_urgency_do_not_change_selection() {
        for satisfaction in [
            astra_turn_types::ResponseSatisfaction::Unknown,
            astra_turn_types::ResponseSatisfaction::Satisfied,
            astra_turn_types::ResponseSatisfaction::Mixed,
            astra_turn_types::ResponseSatisfaction::Dissatisfied,
        ] {
            for urgency in [
                astra_turn_types::TaskUrgency::Unknown,
                astra_turn_types::TaskUrgency::Normal,
                astra_turn_types::TaskUrgency::Urgent,
            ] {
                let assessment = TurnAssessment {
                    difficulty: TaskDifficulty::Easy,
                    difficulty_confidence: AssessmentConfidence::High,
                    satisfaction,
                    urgency,
                    ..Default::default()
                };
                assert_eq!(
                    economy_eligibility(Some(assessment), true, true),
                    ModelRoutingReason::EasyReadOnly
                );
            }
        }
    }
}
