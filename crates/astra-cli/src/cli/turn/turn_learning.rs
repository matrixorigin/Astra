//! Learning and feedback snapshots captured around a turn.

use crate::cli::stream::streaming_types::StreamResult;
use astra_services::session_journal;

pub(crate) struct TurnLearningSnapshot {
    pub evaluation: Option<session_journal::JournalEvent>,
}

pub(crate) fn consume_chat_turn_learning(result: &StreamResult) -> TurnLearningSnapshot {
    TurnLearningSnapshot {
        evaluation: result.turn_evaluation.clone(),
    }
}

impl TurnLearningSnapshot {
    pub(crate) fn succeeded(&self) -> bool {
        self.evaluation
            .as_ref()
            .and_then(|event| event.metadata.as_ref())
            .and_then(|metadata| metadata.get("success"))
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    }

    pub(crate) fn quality_feedback(
        &self,
    ) -> Option<astra_runtime::self_model::TurnQualityFeedback> {
        let event = self.evaluation.as_ref()?;
        let turn = event.turn?;
        let metadata = event.metadata.as_ref()?;
        let signals =
            astra_turn_core::evaluation::turn_evaluation_feedback_signals(metadata).ok()?;
        turn_quality_feedback_from_signals(turn, &signals)
    }
}

pub(crate) fn turn_quality_feedback_from_signals(
    turn: u32,
    signals: &[astra_turn_core::evaluation::EvalSignal],
) -> Option<astra_runtime::self_model::TurnQualityFeedback> {
    use astra_turn_core::evaluation::EvalSignal;
    use std::collections::BTreeSet;

    let mut findings = Vec::new();
    let mut repeated_tools = BTreeSet::new();
    let mut saw_repeat_issue = false;
    let mut saw_batching_issue = false;
    let mut saw_stall_issue = false;

    for signal in signals {
        match signal {
            EvalSignal::RepeatToolCall(tool) => {
                saw_repeat_issue = true;
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
                    "Unresolved tool outcome failure: {class} x{count}. Use the recorded failure to continue the requested work or report what remains unverified."
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

    let recommended_action = match (saw_batching_issue, saw_repeat_issue, saw_stall_issue) {
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
    use super::{consume_chat_turn_learning, turn_quality_feedback_from_signals};
    use astra_services::session_journal;

    #[test]
    fn learning_consumes_server_evidence_despite_partial_client_records() {
        let mut result = crate::tests::stub_stream_result("done");
        result.tool_calls_count = 8;
        result.tool_record_coverage_partial = true;
        result.tool_call_records.clear();
        let missing = consume_chat_turn_learning(&result);
        assert!(missing.evaluation.is_none());
        assert!(!missing.succeeded());
        assert!(missing.quality_feedback().is_none());
        let mut event = session_journal::JournalEvent::turn_evaluation(
            Some("session"),
            Some(9),
            "server_runtime",
            false,
            false,
            0.2,
            0.8,
            0.0,
            0,
            false,
            8,
            vec![astra_turn_core::evaluation::eval_signal_to_json(
                &astra_turn_core::evaluation::EvalSignal::ToolOutcomeFailure {
                    class: "test_failure".into(),
                    count: 2,
                },
            )],
        )
        .with_producer_scope(Some("run"));
        event.metadata.as_mut().unwrap()["status_notice"] =
            serde_json::json!("Server validation remains incomplete");
        let expected = serde_json::to_value(&event).unwrap();
        result.turn_evaluation = Some(event);
        let snapshot = consume_chat_turn_learning(&result);
        assert_eq!(
            serde_json::to_value(snapshot.evaluation.as_ref().unwrap()).unwrap(),
            expected
        );
        assert!(!snapshot.succeeded());
        assert_eq!(snapshot.quality_feedback().unwrap().turn, 9);
        assert!(
            snapshot
                .quality_feedback()
                .unwrap()
                .findings
                .iter()
                .any(|finding| finding.contains("test_failure x2"))
        );
        result
            .tool_call_records
            .push(session_journal::ToolCallRecord {
                name: "bash".into(),
                ok: true,
                ..Default::default()
            });
        assert_eq!(
            serde_json::to_value(
                consume_chat_turn_learning(&result)
                    .evaluation
                    .as_ref()
                    .unwrap()
            )
            .unwrap(),
            expected
        );
    }

    #[test]
    fn turn_quality_feedback_mentions_batching_repeats_and_stalls() {
        use astra_turn_core::evaluation::{EvalSignal, EvaluationThresholds, TurnEvaluation};

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

        let feedback = turn_quality_feedback_from_signals(9, &eval.signals).expect("feedback");
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
        use astra_turn_core::evaluation::{EvalSignal, EvaluationThresholds, TurnEvaluation};

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

        assert!(turn_quality_feedback_from_signals(3, &eval.signals).is_none());
    }
}
