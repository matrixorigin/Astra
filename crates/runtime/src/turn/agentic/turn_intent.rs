use astra_config::user_profile::TurnIntent;
use astra_services::{TurnIntentJudge, TurnIntentJudgeContext, TurnIntentJudgeError};
use std::time::Instant;

/// Judge the user's turn using the supplied LLM judge, falling back to the
/// absence of explicit intent on failure (transport, malformed response, or
/// rejection).
///
/// The LLM judge is the only component allowed to classify natural-language
/// turn intent. Runtime code may still use structural facts (tool history,
/// task type, workspace mutation profile) for routing defaults, but it must
/// not infer semantic intent from keyword lists.
///
/// Telemetry: every judge invocation emits a structured `tracing` event so
/// judge failures and malformed outputs are observable.
pub(crate) async fn judge_turn_intent_with_llm(
    judge: &dyn TurnIntentJudge,
    message: &str,
    turn_count: u32,
    recent_tools: &[String],
    has_prior_assistant_turn: bool,
) -> Option<TurnIntent> {
    let started_at = Instant::now();
    let ctx = TurnIntentJudgeContext {
        message: message.to_string(),
        turn_count,
        recent_tools: recent_tools.to_vec(),
        has_prior_assistant_turn,
    };
    match judge.judge(&ctx).await {
        Ok(intent) => {
            tracing::info!(
                target: "astra::turn_intent",
                operation = "turn_intent.judge",
                source = "llm_judge",
                status = "success",
                duration_ms = started_at.elapsed().as_millis() as u64,
                turn_count,
                has_prior_assistant_turn,
                requested = ?intent.requested_scenario,
                continuation = ?intent.continuation_mode,
                reanchors = intent.reanchors_current_objective,
                "turn intent judged"
            );
            Some(intent)
        }
        Err(error) => {
            let duration_ms = started_at.elapsed().as_millis() as u64;
            // Match the error class to the right severity. Transport errors
            // are operational signal. Malformed responses indicate prompt
            // drift / model regression (raw text is already truncated by the
            // parser). Rejections are the model refusing to answer and are
            // expected to be rare but non-fatal.
            match &error {
                TurnIntentJudgeError::Transport(detail) => tracing::warn!(
                    target: "astra::turn_intent",
                    operation = "turn_intent.judge",
                    source = "llm_judge",
                    status = "error",
                    error_kind = "transport",
                    duration_ms,
                    turn_count,
                    has_prior_assistant_turn,
                    detail = %detail,
                    "turn intent judge transport failure; proceeding without explicit turn intent"
                ),
                TurnIntentJudgeError::Malformed { raw } => tracing::warn!(
                    target: "astra::turn_intent",
                    operation = "turn_intent.judge",
                    source = "llm_judge",
                    status = "error",
                    error_kind = "malformed",
                    duration_ms,
                    turn_count,
                    has_prior_assistant_turn,
                    raw = %raw,
                    "turn intent judge returned malformed response; proceeding without explicit turn intent"
                ),
                TurnIntentJudgeError::Rejected(detail) => tracing::info!(
                    target: "astra::turn_intent",
                    operation = "turn_intent.judge",
                    source = "llm_judge",
                    status = "error",
                    error_kind = "rejected",
                    duration_ms,
                    turn_count,
                    has_prior_assistant_turn,
                    detail = %detail,
                    "turn intent judge rejected request; proceeding without explicit turn intent"
                ),
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::judge_turn_intent_with_llm;
    use astra_config::user_profile::{TurnContinuationMode, TurnIntent};
    use astra_services::{TurnIntentJudge, TurnIntentJudgeContext, TurnIntentJudgeError};
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
    async fn judge_success_returns_structured_intent() {
        let message = "可以了，按你刚才说的方向继续往下走";
        let llm_intent = TurnIntent::default()
            .with_continuation_mode(TurnContinuationMode::ContinueCurrentObjective);
        let judge = FixedJudge::ok(llm_intent.clone());

        let out =
            judge_turn_intent_with_llm(&judge, message, 5, &["read_file".to_string()], true).await;
        assert_eq!(
            out,
            Some(llm_intent),
            "LLM judge result must be the structured turn intent"
        );
    }

    #[tokio::test]
    async fn judge_transport_failure_returns_none() {
        let message = "please inspect the current changes";

        let judge = FixedJudge::err(TurnIntentJudgeError::Transport("connection reset".into()));
        assert_eq!(
            judge_turn_intent_with_llm(&judge, message, 1, &[], false).await,
            None
        );
    }

    #[tokio::test]
    async fn judge_malformed_response_returns_none() {
        let message = "why is this test failing?";
        let judge = FixedJudge::err(TurnIntentJudgeError::Malformed {
            raw: "garbled".into(),
        });
        assert_eq!(
            judge_turn_intent_with_llm(&judge, message, 2, &[], true).await,
            None
        );
    }

    #[tokio::test]
    async fn judge_failure_returns_none_when_keyword_also_has_no_signal() {
        let message = "x";
        let judge = FixedJudge::err(TurnIntentJudgeError::Rejected("no model".into()));
        let out = judge_turn_intent_with_llm(&judge, message, 1, &[], false).await;
        assert!(out.is_none(), "judge failure must return None, got {out:?}");
    }
}
