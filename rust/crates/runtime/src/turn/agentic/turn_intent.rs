use astra_config::user_profile::{Scenario, TurnContinuationMode, TurnIntent};
use astra_services::{TurnIntentJudge, TurnIntentJudgeContext, TurnIntentJudgeError};

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
            // Match the error class to the right severity. Transport errors
            // are operational signal. Malformed responses indicate prompt
            // drift / model regression (raw text is already truncated by the
            // parser). Rejections are the model refusing to answer and are
            // expected to be rare but non-fatal.
            match &error {
                TurnIntentJudgeError::Transport(detail) => tracing::warn!(
                    target: "astra::turn_intent",
                    source = "llm_judge",
                    error_kind = "transport",
                    detail = %detail,
                    "turn intent judge transport failure; proceeding without explicit turn intent"
                ),
                TurnIntentJudgeError::Malformed { raw } => tracing::warn!(
                    target: "astra::turn_intent",
                    source = "llm_judge",
                    error_kind = "malformed",
                    raw = %raw,
                    "turn intent judge returned malformed response; proceeding without explicit turn intent"
                ),
                TurnIntentJudgeError::Rejected(detail) => tracing::info!(
                    target: "astra::turn_intent",
                    source = "llm_judge",
                    error_kind = "rejected",
                    detail = %detail,
                    "turn intent judge rejected request; proceeding without explicit turn intent"
                ),
            }
            None
        }
    }
}

/// Structural fallback used when the LLM judge is unavailable, errors, or
/// returns a malformed response. Without this, every judge failure collapses
/// to `None` and downstream code loses *all* intent signal — scenario routing,
/// continuation routing, and adaptive profiles all degrade to defaults. This
/// keeps the loop functional under judge outages.
///
/// First-principles: the fallback must never *fabricate* a scenario it cannot
/// derive from structural evidence. It infers only what the tool history and
/// message shape support, and otherwise returns a minimal intent that signals
/// "unknown scenario, continue if context suggests it".
pub(crate) fn fallback_turn_intent(
    message: &str,
    recent_tools: &[String],
    has_prior_assistant_turn: bool,
) -> TurnIntent {
    use astra_turn_core::input_classifier::is_correction_signal;

    // 1. Correction signal → user is redirecting the *current* objective,
    //    not starting a new one. High-precision keyword signal we still trust.
    if is_correction_signal(message) {
        return TurnIntent::default()
            .with_continuation_mode(TurnContinuationMode::ContinueCurrentObjective);
    }

    // 2. Short follow-up after an assistant turn with no new objective
    //    language → treat as continuation. Avoids the failure mode where a
    //    bare "yes" / "继续" / "go ahead" resets the scenario to Unknown.
    let trimmed = message.trim();
    let is_short_followup = trimmed.chars().count() <= 24 && has_prior_assistant_turn;
    if is_short_followup {
        return TurnIntent::default()
            .with_continuation_mode(TurnContinuationMode::ContinueCurrentObjective);
    }

    // 3. Infer scenario from recent tool history. Tool choices are a strong,
    //    honest signal of what the user is actually doing — stronger than
    //    message keywords. Map the dominant tool family to a scenario.
    let inferred = infer_scenario_from_tools(recent_tools);
    let mut intent = TurnIntent::default();
    if let Some(scenario) = inferred {
        intent = intent.with_requested_scenario(scenario);
    }
    // 4. If there is prior assistant context and we couldn't classify, prefer
    //    continuation over NewObjective — a wrong NewObjective resets budgets
    //    and adaptive state mid-task, which is the more costly error.
    if has_prior_assistant_turn && intent.requested_scenario.is_none() {
        intent = intent.with_continuation_mode(TurnContinuationMode::ContinueCurrentObjective);
    }
    intent
}

/// Map a tool history profile to the single most-likely scenario. Returns
/// `None` when the tool mix is empty or ambiguous — the caller decides the
/// default, not this heuristic.
fn infer_scenario_from_tools(recent_tools: &[String]) -> Option<Scenario> {
    if recent_tools.is_empty() {
        return None;
    }
    let mut edit = 0usize;
    let mut inspect = 0usize;
    let mut search = 0usize;
    let mut test = 0usize;
    for t in recent_tools {
        let t = t.as_str();
        if matches!(
            t,
            "edit" | "str_replace" | "write_file" | "create" | "apply_patch"
        ) {
            edit += 1;
        } else if matches!(t, "bash" | "view" | "read_file") {
            inspect += 1;
        } else if matches!(t, "grep" | "glob" | "web_search" | "list_dir") {
            search += 1;
        } else if matches!(t, "test" | "cargo_test" | "pytest" | "jest") {
            test += 1;
        }
    }
    if test > 0 && test >= edit {
        return Some(Scenario::Testing);
    }
    if edit >= inspect && edit > 0 {
        return Some(Scenario::Implementation);
    }
    if search > inspect && search > 0 {
        return Some(Scenario::Exploration);
    }
    if inspect > 0 {
        return Some(Scenario::Debugging);
    }
    None
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

    // --- fallback_turn_intent direct coverage ---
    //
    // The judge path above returns `None` on failure; `fallback_turn_intent`
    // is what keeps the loop functional under judge outages. It must infer
    // only from structural evidence and never fabricate a scenario.

    use super::{fallback_turn_intent, infer_scenario_from_tools};
    use astra_config::user_profile::Scenario;

    #[test]
    fn fallback_correction_signal_forces_continue_current() {
        // "不对" / "stop" style redirections must route to continuation of the
        // current objective, not a fresh NewObjective that resets budgets.
        let intent =
            fallback_turn_intent("不对，刚才那个改错了", &["str_replace".to_string()], true);
        assert_eq!(
            intent.continuation_mode,
            TurnContinuationMode::ContinueCurrentObjective
        );
    }

    #[test]
    fn fallback_short_followup_after_assistant_continues() {
        // A bare "继续" / "yes" / "go" right after an assistant turn should not
        // be treated as a new objective — that would wipe adaptive state.
        let intent = fallback_turn_intent("继续", &[], true);
        assert_eq!(
            intent.continuation_mode,
            TurnContinuationMode::ContinueCurrentObjective
        );
        assert!(
            intent.requested_scenario.is_none(),
            "no tool evidence → no fabricated scenario"
        );
    }

    #[test]
    fn fallback_short_followup_without_prior_assistant_does_not_continue() {
        // First turn of a session: even a short message has nothing to continue.
        let intent = fallback_turn_intent("hi", &[], false);
        assert_ne!(
            intent.continuation_mode,
            TurnContinuationMode::ContinueCurrentObjective
        );
    }

    // --- infer_scenario_from_tools: direct coverage ---
    //
    // Scenario inference is a pure function over tool history. Testing it
    // directly avoids the short-followup early-return in `fallback_turn_intent`
    // (messages ≤24 chars with a prior assistant turn never reach the
    // inference path). The fallback behavioral tests above cover that branch.

    #[test]
    fn infer_scenario_edit_tools_yield_implementation() {
        assert_eq!(
            infer_scenario_from_tools(&["str_replace".to_string(), "write_file".to_string()]),
            Some(Scenario::Implementation)
        );
    }

    #[test]
    fn infer_scenario_test_wins_over_edit_on_tie() {
        // `test >= edit` → testing takes priority: the user has moved past
        // implementation into verification.
        assert_eq!(
            infer_scenario_from_tools(&["str_replace".to_string(), "cargo_test".to_string()]),
            Some(Scenario::Testing)
        );
    }

    #[test]
    fn infer_scenario_search_tools_yield_exploration() {
        assert_eq!(
            infer_scenario_from_tools(&["grep".to_string(), "glob".to_string()]),
            Some(Scenario::Exploration)
        );
    }

    #[test]
    fn infer_scenario_inspect_tools_yield_debugging() {
        assert_eq!(
            infer_scenario_from_tools(&["read_file".to_string(), "bash".to_string()]),
            Some(Scenario::Debugging)
        );
    }

    #[test]
    fn infer_scenario_edit_wins_tie_over_inspect() {
        // `edit >= inspect` (not strict `>`) — on a tie, implementation wins.
        assert_eq!(
            infer_scenario_from_tools(&["str_replace".to_string(), "read_file".to_string()]),
            Some(Scenario::Implementation)
        );
    }

    #[test]
    fn infer_scenario_empty_tools_is_none() {
        assert_eq!(infer_scenario_from_tools(&[]), None);
    }

    #[test]
    fn infer_scenario_unmatched_tools_is_none() {
        // Tools matching no known family produce no confident scenario claim.
        assert_eq!(
            infer_scenario_from_tools(&["unknown_tool".to_string()]),
            None
        );
    }
}
