use super::*;
use crate::StreamResult;

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
        TurnEvaluationTelemetry, current_evaluation_thresholds,
        evaluate_tool_call_records_with_thresholds_and_telemetry,
    };
    use astra_turn_core::routing_engine::RoutingEngine;

    let routing = RoutingEngine::analyze(line, turn, recent_tools, &[], vec![]);

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

    let eval = evaluate_tool_call_records_with_thresholds_and_telemetry(
        line,
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
            EvalSignal::SequentialReadChurn(streak) => {
                saw_batching_issue = true;
                findings.push(format!(
                    "Detected {streak} consecutive single-tool rounds; independent reads/searches should be batched in one round."
                ));
            }
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
    use super::*;

    fn stub_stream_result(full_text: &str) -> StreamResult {
        StreamResult {
            session_id: None,
            run_id: None,
            session_persistence_error: None,
            full_text: full_text.to_string(),
            tool_calls_count: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tools_selected: Vec::new(),
            selected_skills: Vec::new(),
            tools_used: Vec::new(),
            tool_call_records: Vec::new(),
            budget_used: 0,
            budget_pressure: 0.0,
            stall_events: Vec::new(),
            verdict_events: Vec::new(),
            step_recorder_summary: None,
            tool_health_export: Vec::new(),
            last_heavy_checkpoint: None,
            ttft_ms: None,
            context_ms: None,
            memoria_ms: None,
            routing_domain_hint: None,
            entity_learn_skipped_no_domain: false,
            pending_context_assembly_trace: None,
            turn_observability_events: Vec::new(),
            llm_rounds: None,
            interruption: None,
            final_state: "completed".into(),
            interruption_kind: None,
            final_messages: Vec::new(),
            background_agent_results: Vec::new(),
        }
    }

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
        let mut result = stub_stream_result("");
        result.tools_used = vec!["git_diff".into()];
        result.tool_calls_count = 1;
        result.prompt_tokens = 136_947;
        result.llm_rounds = Some(9);
        result.turn_observability_events =
            vec![llm_round_event(0, 9_401), llm_round_event(7, 20_954)];
        result.tool_call_records = vec![session_journal::ToolCallRecord {
            name: "git_diff".into(),
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
    fn turn_quality_feedback_mentions_batching_repeats_and_stalls() {
        use astra_runtime::pipeline::evaluation::{
            EvalSignal, EvaluationThresholds, TurnEvaluation,
        };

        let eval = TurnEvaluation {
            success: false,
            quality: 0.2,
            confidence: 0.8,
            signals: vec![
                EvalSignal::SequentialReadChurn(13),
                EvalSignal::RepeatToolCall("bash".to_string()),
                EvalSignal::RepeatToolCall("read_file".to_string()),
                EvalSignal::StallDetected,
                EvalSignal::VerdictWarning,
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
