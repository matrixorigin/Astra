use astra_config::user_profile::{Scenario, TurnContinuationMode, TurnIntent};
use astra_turn_core::chat_turn_heuristics::TaskExecutionProfile;
use astra_turn_core::input_classifier::{TurnScenarioHint, classify_turn_input};

pub(crate) fn infer_turn_intent(
    message: &str,
    task_profile: TaskExecutionProfile,
) -> Option<TurnIntent> {
    let signals = classify_turn_input(message, task_profile);
    if !signals.has_signal() {
        return None;
    }

    let mut intent = TurnIntent::default();
    if signals.prohibit_code_review {
        intent = intent.prohibit_scenario(Scenario::CodeReview);
    }

    if signals.continue_current_objective {
        intent = intent.with_continuation_mode(TurnContinuationMode::ContinueCurrentObjective);
    } else if let Some(scenario_hint) = signals.scenario_hint {
        intent = intent.with_requested_scenario(match scenario_hint {
            TurnScenarioHint::CodeReview => Scenario::CodeReview,
            TurnScenarioHint::Debugging => Scenario::Debugging,
            TurnScenarioHint::QuickAnswer => Scenario::QuickAnswer,
        });
    }

    Some(intent)
}

#[cfg(test)]
mod tests {
    use super::infer_turn_intent;
    use astra_config::user_profile::{Scenario, TurnContinuationMode};
    use astra_turn_core::chat_turn_heuristics::infer_task_execution_profile;

    #[test]
    fn infers_code_review_for_change_inspection() {
        let message = "please inspect the current changes";
        let intent = infer_turn_intent(message, infer_task_execution_profile(message))
            .expect("code review intent");

        assert_eq!(intent.requested_scenario, Some(Scenario::CodeReview));
    }

    #[test]
    fn infers_debugging_for_failure_question() {
        let message = "why is this test failing?";
        let intent = infer_turn_intent(message, infer_task_execution_profile(message))
            .expect("debugging intent");

        assert_eq!(intent.requested_scenario, Some(Scenario::Debugging));
    }

    #[test]
    fn infers_quick_answer_for_short_read_only_question() {
        let message = "where is the auth flow defined?";
        let intent = infer_turn_intent(message, infer_task_execution_profile(message))
            .expect("quick answer intent");

        assert_eq!(intent.requested_scenario, Some(Scenario::QuickAnswer));
    }

    #[test]
    fn does_not_route_mutating_question_to_quick_answer() {
        let message = "fix it?";
        let intent = infer_turn_intent(message, infer_task_execution_profile(message))
            .expect("continuation intent");

        assert_ne!(intent.requested_scenario, Some(Scenario::QuickAnswer));
        assert_eq!(
            intent.continuation_mode,
            TurnContinuationMode::ContinueCurrentObjective
        );
    }

    #[test]
    fn infers_review_prohibition_without_requesting_review() {
        let message = "don't review this, just continue the implementation";
        let intent = infer_turn_intent(message, infer_task_execution_profile(message))
            .expect("review prohibition");

        assert!(intent.allows_scenario(Scenario::Implementation));
        assert!(!intent.allows_scenario(Scenario::CodeReview));
        assert_ne!(intent.requested_scenario, Some(Scenario::CodeReview));
    }

    #[test]
    fn infers_low_info_continuation() {
        let message = "继续";
        let intent = infer_turn_intent(message, infer_task_execution_profile(message))
            .expect("continuation");

        assert_eq!(
            intent.continuation_mode,
            TurnContinuationMode::ContinueCurrentObjective
        );
    }
}
