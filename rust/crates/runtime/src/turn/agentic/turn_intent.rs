use astra_config::user_profile::{Scenario, TurnContinuationMode, TurnIntent};
use astra_services::{TurnIntentJudge, TurnIntentJudgeContext, TurnIntentJudgeError};
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

/// Judge the user's turn using the supplied LLM judge, falling back to the
/// deterministic keyword classifier on failure (transport, malformed
/// response, or rejection).
///
/// Why both: the LLM judge handles paraphrases, indirect speech, and mixed
/// language correctly — cases the keyword classifier misses. But the loop
/// must never block on a transient LLM outage, so a bounded timeout +
/// keyword fallback is the safety net.
///
/// Telemetry: every judge invocation emits a structured `tracing` event so
/// drift between LLM and keyword classifications is observable. The keyword
/// path is logged at `debug` (the common case); judge failures at `warn`.
pub(crate) async fn judge_turn_intent_with_llm_fallback(
    judge: &dyn TurnIntentJudge,
    message: &str,
    task_profile: TaskExecutionProfile,
    turn_count: u32,
    recent_tools: &[String],
    has_prior_assistant_turn: bool,
) -> Option<TurnIntent> {
    let ctx = TurnIntentJudgeContext {
        message: message.to_string(),
        turn_count,
        recent_tools: recent_tools.to_vec(),
        has_prior_assistant_turn,
    };
    match judge.judge(&ctx).await {
        Ok(intent) => {
            tracing::debug!(
                target: "astra::turn_intent",
                source = "llm_judge",
                requested = ?intent.requested_scenario,
                continuation = ?intent.continuation_mode,
                "turn intent judged"
            );
            Some(intent)
        }
        Err(error) => {
            // Match the error class to the right severity. Transport
            // errors are operational signal (log at warn). Malformed
            // responses indicate prompt drift / model regression (warn
            // with the raw text already truncated by the parser).
            // Rejections are the model refusing to answer — log at info
            // since the keyword fallback is by-design and not a bug.
            match &error {
                TurnIntentJudgeError::Transport(detail) => tracing::warn!(
                    target: "astra::turn_intent",
                    source = "llm_judge",
                    error_kind = "transport",
                    detail = %detail,
                    "turn intent judge transport failure; falling back to keyword classifier"
                ),
                TurnIntentJudgeError::Malformed { raw } => tracing::warn!(
                    target: "astra::turn_intent",
                    source = "llm_judge",
                    error_kind = "malformed",
                    raw = %raw,
                    "turn intent judge returned malformed response; falling back to keyword classifier"
                ),
                TurnIntentJudgeError::Rejected(detail) => tracing::info!(
                    target: "astra::turn_intent",
                    source = "llm_judge",
                    error_kind = "rejected",
                    detail = %detail,
                    "turn intent judge rejected request; falling back to keyword classifier"
                ),
            }
            infer_turn_intent(message, task_profile)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{infer_turn_intent, judge_turn_intent_with_llm_fallback};
    use astra_config::user_profile::{Scenario, TurnContinuationMode, TurnIntent};
    use astra_services::{TurnIntentJudge, TurnIntentJudgeContext, TurnIntentJudgeError};
    use astra_turn_core::chat_turn_heuristics::infer_task_execution_profile;
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct FixedJudge {
        result: Mutex<Option<Result<TurnIntent, TurnIntentJudgeError>>>,
    }

    impl FixedJudge {
        fn ok(intent: TurnIntent) -> Self {
            Self {
                result: Mutex::new(Some(Ok(intent))),
            }
        }
        fn err(error: TurnIntentJudgeError) -> Self {
            Self {
                result: Mutex::new(Some(Err(error))),
            }
        }
    }

    #[async_trait]
    impl TurnIntentJudge for FixedJudge {
        async fn judge(
            &self,
            _ctx: &TurnIntentJudgeContext,
        ) -> Result<TurnIntent, TurnIntentJudgeError> {
            self.result
                .lock()
                .unwrap()
                .take()
                .expect("FixedJudge consumed twice")
        }
    }

    #[tokio::test]
    async fn judge_success_overrides_keyword_fallback() {
        // Message has no keyword signal — the deterministic classifier
        // returns None — but the LLM judge produces a clear intent.
        // Verify the helper returns the LLM result, not the keyword
        // None.
        let message = "可以了，按你刚才说的方向继续往下走";
        let task_profile = infer_task_execution_profile(message);
        // Sanity: keyword classifier alone might miss this paraphrase,
        // and even if it returned a default that happens to match, the
        // judge result wins.
        let llm_intent = TurnIntent::default()
            .with_continuation_mode(TurnContinuationMode::ContinueCurrentObjective);
        let judge = FixedJudge::ok(llm_intent.clone());

        let out = judge_turn_intent_with_llm_fallback(
            &judge,
            message,
            task_profile,
            5,
            &["read_file".to_string()],
            true,
        )
        .await;
        assert_eq!(
            out,
            Some(llm_intent),
            "LLM judge result must win over keyword fallback when judge succeeds"
        );
    }

    #[tokio::test]
    async fn judge_transport_failure_falls_back_to_keyword_classifier() {
        // Message that the keyword classifier WILL detect as code review.
        let message = "please inspect the current changes";
        let task_profile = infer_task_execution_profile(message);

        let judge = FixedJudge::err(TurnIntentJudgeError::Transport(
            "connection reset".into(),
        ));
        let out =
            judge_turn_intent_with_llm_fallback(&judge, message, task_profile, 1, &[], false)
                .await;
        let intent = out.expect("fallback must produce an intent");
        assert_eq!(
            intent.requested_scenario,
            Some(Scenario::CodeReview),
            "transport failure must fall back to deterministic keyword classifier"
        );
    }

    #[tokio::test]
    async fn judge_malformed_response_falls_back_to_keyword_classifier() {
        let message = "why is this test failing?";
        let task_profile = infer_task_execution_profile(message);
        let judge = FixedJudge::err(TurnIntentJudgeError::Malformed {
            raw: "garbled".into(),
        });
        let out =
            judge_turn_intent_with_llm_fallback(&judge, message, task_profile, 2, &[], true)
                .await;
        let intent = out.expect("fallback must produce an intent");
        assert_eq!(intent.requested_scenario, Some(Scenario::Debugging));
    }

    #[tokio::test]
    async fn judge_failure_returns_none_when_keyword_also_has_no_signal() {
        // Adversarial: judge errors AND the message has no keyword
        // signal. The helper must return None — not a default intent —
        // so the loop falls back to its scenario-by-task-profile path.
        let message = "x";
        let task_profile = infer_task_execution_profile(message);
        let judge = FixedJudge::err(TurnIntentJudgeError::Rejected("no model".into()));
        let out =
            judge_turn_intent_with_llm_fallback(&judge, message, task_profile, 1, &[], false)
                .await;
        assert!(
            out.is_none(),
            "with no keyword signal, judge failure must return None, got {out:?}"
        );
    }


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
