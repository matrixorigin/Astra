//! Learning and feedback snapshots captured around a turn.

use crate::cli::stream::streaming_types::StreamResult;
use astra_services::session_journal;

pub(crate) struct TurnLearningSnapshot {
    pub routing: astra_turn_core::routing_engine::RoutingDecision,
    pub eval: astra_runtime::pipeline::evaluation::TurnEvaluation,
}

pub(crate) fn analyze_chat_turn_learning(
    line: &str,
    turn: u32,
    recent_tools: &[String],
    result: &StreamResult,
) -> TurnLearningSnapshot {
    use astra_runtime::pipeline::evaluation::{
        TurnEvaluationTelemetry, apply_final_answer_relevance, current_evaluation_thresholds,
        evaluate_tool_call_records_with_thresholds_and_telemetry,
    };
    use astra_turn_core::routing_engine::RoutingEngine;

    let latest_user_input = result.latest_user_input(line);
    let routing = RoutingEngine::analyze(&latest_user_input, turn, recent_tools, &[], vec![]);

    let has_verdict_warning = result.verdict_events.iter().any(|verdict| {
        verdict.severity.eq_ignore_ascii_case("warning")
            || verdict.severity.eq_ignore_ascii_case("critical")
    });

    let mut first_round_prompt_tokens: Option<u64> = None;
    let mut max_round_prompt_tokens: Option<u64> = None;
    for event in &result.turn_observability_events {
        if event.event_type != session_journal::JournalEventType::LlmRound {
            continue;
        }
        let source = event
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("source"))
            .and_then(serde_json::Value::as_str);
        if source != Some("agentic_loop") {
            continue;
        }
        let Some(tokens_in) = event.tokens_in else {
            continue;
        };
        first_round_prompt_tokens.get_or_insert(tokens_in);
        max_round_prompt_tokens = Some(
            max_round_prompt_tokens
                .map(|current| current.max(tokens_in))
                .unwrap_or(tokens_in),
        );
    }

    let mut eval = evaluate_tool_call_records_with_thresholds_and_telemetry(
        &latest_user_input,
        recent_tools,
        &result.tool_call_records,
        result.stall_events.len(),
        has_verdict_warning,
        result.budget_pressure,
        current_evaluation_thresholds(),
        TurnEvaluationTelemetry {
            llm_rounds: result.llm_rounds,
            prompt_tokens: Some(result.prompt_tokens),
            first_round_prompt_tokens,
            max_round_prompt_tokens,
        },
    );
    apply_final_answer_relevance(&mut eval, &latest_user_input, &result.full_text);

    TurnLearningSnapshot { routing, eval }
}

pub(crate) fn turn_quality_feedback_from_eval(
    turn: u32,
    eval: &astra_runtime::pipeline::evaluation::TurnEvaluation,
) -> Option<astra_runtime::self_model::TurnQualityFeedback> {
    use astra_runtime::pipeline::evaluation::EvalSignal;
    use std::collections::BTreeSet;

    let mut findings = Vec::new();
    let mut repeated_tools = BTreeSet::new();
    let mut saw_batching_issue = false;
    let mut saw_stall_issue = false;

    for signal in &eval.signals {
        match signal {
            EvalSignal::RepeatToolCall(tool) => {
                repeated_tools.insert(tool.clone());
            }
            EvalSignal::StallDetected => {
                saw_stall_issue = true;
                findings.push(
                    "TurnGuard detected a stall/divergence; stop broad exploration and take a concrete next action."
                        .to_string(),
                );
            }
            EvalSignal::VerdictWarning => {
                saw_stall_issue = true;
                findings.push(
                    "TurnGuard emitted a warning-or-higher verdict; follow that warning instead of continuing the same pattern."
                        .to_string(),
                );
            }
            EvalSignal::ToolOutcomeFailure { class, count } => {
                saw_stall_issue = true;
                findings.push(format!(
                    "Unresolved tool outcome failure: {class} x{count}; do not report completion until a matching validation command succeeds."
                ));
            }
            EvalSignal::BlockedToolCall { count } => {
                saw_stall_issue = true;
                findings.push(format!(
                    "{count} tool call(s) were blocked before execution; do not retry the same unavailable tool surface without changing provider or approach."
                ));
            }
            EvalSignal::ExplorationFamilyChurn { streak, .. } => {
                saw_batching_issue = true;
                findings.push(format!(
                    "{streak} consecutive reads in a single tool family — batch them in one round instead."
                ));
            }
            _ => {}
        }
    }

    if !repeated_tools.is_empty() {
        findings.push(format!(
            "Repeated tool calls without new evidence: {}.",
            repeated_tools.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }

    if findings.is_empty() {
        return None;
    }

    let recommended_action = match (
        saw_batching_issue,
        findings.iter().any(|f| f.contains("Repeated tool calls")),
        saw_stall_issue,
    ) {
        (true, true, true) => {
            "Batch independent reads/searches, reuse previous tool output before repeating calls, then choose one concrete recovery action."
        }
        (true, true, false) => {
            "Batch independent reads/searches in one round and reuse previous output before repeating a tool call."
        }
        (true, false, _) => {
            "Before the next tool round, group independent reads/searches into a parallel batch."
        }
        (false, true, _) => {
            "Before retrying a tool, compare against prior output and change arguments only when new evidence requires it."
        }
        (false, false, true) => {
            "Summarize current evidence, stop broad exploration, and take one concrete next action."
        }
        (false, false, false) => unreachable!("findings would be empty without a tracked issue"),
    };

    Some(astra_runtime::self_model::TurnQualityFeedback {
        turn,
        findings,
        recommended_action: recommended_action.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::{analyze_chat_turn_learning, turn_quality_feedback_from_eval};
    use astra_services::session_journal;

    #[test]
    fn analyze_chat_turn_learning_flags_llm_round_churn() {
        let llm_round_event = |round: u32, tokens_in: u64| {
            let mut event = session_journal::JournalEvent::base_public(
                session_journal::JournalEventType::LlmRound,
                Some("sess-1"),
            );
            event.turn = Some(2);
            event.round = Some(round);
            event.tokens_in = Some(tokens_in);
            event.metadata = Some(serde_json::json!({
                "source": "agentic_loop",
            }));
            event
        };
        let mut result = crate::tests::stub_stream_result("");
        result.tools_used = vec!["git".into()];
        result.tool_calls_count = 1;
        result.prompt_tokens = 136_947;
        result.llm_rounds = Some(9);
        result.turn_observability_events =
            vec![llm_round_event(0, 9_401), llm_round_event(7, 20_954)];
        result.tool_call_records = vec![session_journal::ToolCallRecord {
            name: "git".into(),
            ok: true,
            ms: 12,
            error: None,
            input_bytes: Some(16),
            output_bytes: Some(240),
            args_preview: None,
            result_preview: Some("diff".into()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }];

        let learning = analyze_chat_turn_learning("review local changes", 2, &[], &result);
        assert!(learning.eval.signals.iter().any(|signal| matches!(
            signal,
            astra_runtime::pipeline::evaluation::EvalSignal::LlmRoundChurn {
                rounds: 9,
                prompt_tokens: 136_947,
            }
        )));
        assert!(learning.eval.signals.iter().any(|signal| matches!(
            signal,
            astra_runtime::pipeline::evaluation::EvalSignal::PromptGrowthChurn {
                first_prompt_tokens: 9_401,
                max_prompt_tokens: 20_954,
                delta_tokens: 11_553,
            }
        )));
    }

    #[test]
    fn analyze_chat_turn_learning_applies_final_answer_relevance() {
        let mut result =
            crate::tests::stub_stream_result("148 files changed, +9498 / -2335 lines, 11 commits.");
        result.tools_used = vec!["git".into()];
        result.tool_calls_count = 1;
        result.tool_call_records = vec![session_journal::ToolCallRecord {
            name: "git".into(),
            ok: true,
            ms: 12,
            output_bytes: Some(240),
            result_preview: Some("diff".into()),
            ..Default::default()
        }];

        let learning = analyze_chat_turn_learning("相关的测试够硬核吗？", 3, &[], &result);

        assert!(!learning.eval.success);
        assert!(learning.eval.signals.iter().any(|signal| matches!(
            signal,
            astra_runtime::pipeline::evaluation::EvalSignal::FinalAnswerOffTarget { .. }
        )));
        assert!(
            !learning.eval.signals.iter().any(|signal| matches!(
                signal,
                astra_runtime::pipeline::evaluation::EvalSignal::AllToolsHealthy
            )),
            "tool health must not mask an off-target answer"
        );
    }

    #[test]
    fn turn_quality_feedback_mentions_batching_repeats_and_stalls() {
        use astra_runtime::pipeline::evaluation::{
            EvalSignal, EvaluationThresholds, TurnEvaluation,
        };

        let eval = TurnEvaluation {
            success: false,
            quality: 0.2,
            confidence: 0.8,
            signals: vec![
                EvalSignal::ExplorationFamilyChurn {
                    family: "read".to_string(),
                    streak: 13,
                },
                EvalSignal::RepeatToolCall("bash".to_string()),
                EvalSignal::RepeatToolCall("read_file".to_string()),
                EvalSignal::StallDetected,
                EvalSignal::VerdictWarning,
                EvalSignal::ToolOutcomeFailure {
                    class: "test_failure".to_string(),
                    count: 1,
                },
            ],
            thresholds: EvaluationThresholds::default(),
        };

        let feedback = turn_quality_feedback_from_eval(9, &eval).expect("feedback");
        assert_eq!(feedback.turn, 9);
        assert!(
            feedback
                .findings
                .iter()
                .any(|finding| finding.contains("13 consecutive"))
        );
        assert!(
            feedback
                .findings
                .iter()
                .any(|finding| finding.contains("bash") && finding.contains("read_file"))
        );
        assert!(
            feedback
                .findings
                .iter()
                .any(|finding| finding.contains("test_failure"))
        );
        assert!(feedback.recommended_action.contains("Batch independent"));
    }

    #[test]
    fn turn_quality_feedback_ignores_untracked_or_empty_signals() {
        use astra_runtime::pipeline::evaluation::{
            EvalSignal, EvaluationThresholds, TurnEvaluation,
        };

        let eval = TurnEvaluation {
            success: true,
            quality: 0.8,
            confidence: 0.7,
            signals: vec![
                EvalSignal::ToolErrorRate(0.0),
                EvalSignal::AllToolsHealthy,
                EvalSignal::EmptyToolOutput,
            ],
            thresholds: EvaluationThresholds::default(),
        };

        assert!(turn_quality_feedback_from_eval(3, &eval).is_none());
    }
}
