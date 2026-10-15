//! Secret-free, incremental projection of the existing suite event stream.
use crate::{
    criteria::{Criterion, CriterionResult},
    dashboard::DashboardEvent,
    report::CaseRunReport,
    runner::RunOutcome,
};
use serde_json::{Value, json};
use std::io::{self, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::broadcast;

pub const SCHEMA: &str = "astra.harness_progress.v2";
pub const MAX_RECORD_BYTES: usize = 262_144;

fn criteria(values: &[CriterionResult], outcome: &RunOutcome) -> io::Result<Vec<Value>> {
    values
        .iter()
        .map(|c| {
            if c.score.is_some_and(|s| !s.is_finite()) {
                return Err(io::Error::other("invalid progress score"));
            }
            let tag = serde_json::to_value(&c.criterion)?["type"].clone();
            let advisory = matches!(c.criterion, Criterion::Judger { .. });
            let judge = advisory || matches!(c.criterion, Criterion::HardJudger { .. });
            let diagnostic = match &c.criterion {
                Criterion::ExecutionToolCount { .. }
                | Criterion::ExecutionToolSequence { .. }
                | Criterion::ExecutionChildCount { .. }
                | Criterion::ExecutionChildResultsAdopted { .. } =>
                    crate::criteria::execution_contract_proof(&c.criterion, outcome)
                        .map(|(_, diagnostic)| diagnostic)
                        .unwrap_or_else(|_| json!({"kind":"unavailable"})),
                Criterion::DurationBetween { min_ms, max_ms } =>
                    json!({"kind":"count", "min":min_ms, "max":max_ms, "actual":outcome.duration_ms}),
                Criterion::ToolsCountBetween { min, max } =>
                    json!({"kind":"count", "min":min, "max":max, "actual":outcome.tool_calls_count}),
                Criterion::TurnRoundsBetween { min, max } =>
                    json!({"kind":"count", "min":min, "max":max, "actual":outcome.turn_rounds}),
                Criterion::ExitCode { code } =>
                    json!({"kind":"exit", "expected":code, "actual":outcome.exit_code}),
                Criterion::Judger { threshold, .. } | Criterion::HardJudger { threshold, .. } =>
                    json!({"kind":"judge", "threshold":threshold, "assessment":crate::judger::assessment_status(c)}),
                Criterion::AllOf { .. } | Criterion::AnyOf { .. } => {
                    let nested = c.full_detail.as_deref()
                        .and_then(|raw| serde_json::from_str::<Vec<CriterionResult>>(raw).ok());
                    match nested {
                        Some(nested) => json!({"kind":"composite", "checks":criteria(&nested, outcome)?}),
                        None => Value::Null,
                    }
                }
                _ => Value::Null,
            };
            Ok(json!({"type":tag, "severity":c.severity, "score":c.score,
            "diagnostic":diagnostic,
            "passed": if advisory && c.score.is_none() {None} else {Some(c.passed)},
            "evaluation": if judge && c.score.is_some() {"scored"}
                else if advisory {"unscored"} else {"reported"}}))
        })
        .collect()
}

fn row(report: &CaseRunReport) -> io::Result<Value> {
    let o = &report.outcome;
    let failure = serde_json::to_value(&report.failure_class)?;
    let failure = if let Some(fields) = failure.as_object() {
        json!(fields.keys().next())
    } else {
        failure
    };
    let steps: io::Result<Vec<_>> = report
        .steps
        .iter()
        .map(|s| {
            Ok(json!({
                "step_index":s.step_index, "passed":s.passed, "duration_ms":s.duration_ms,
                "criteria":criteria(&s.criteria, &s.outcome)?
            }))
        })
        .collect();
    Ok(
        json!({"case":report.case_name, "model":report.model, "run_index":report.run_index,
        "status":report.status, "has_warnings":report.has_warnings,
        "failure_class":failure, "exit_code":o.exit_code,
        "criteria":criteria(&report.criteria, o)?, "steps":steps?,
        "token_usage_coverage":o.token_usage_coverage,
        "terminal_reported":{"duration_ms":o.duration_ms, "ttft_ms":o.ttft_ms,
            "turn_rounds":o.turn_rounds, "tool_calls_count":o.tool_calls_count,
            "prompt_tokens":o.prompt_tokens, "cached_input_tokens":o.cached_input_tokens,
            "cache_creation_tokens":o.cache_creation_tokens, "completion_tokens":o.completion_tokens}}),
    )
}

fn project(event: &DashboardEvent) -> io::Result<Value> {
    let (run, seq, mut value) = match event {
        DashboardEvent::SuiteStarted {
            run_id,
            sequence,
            total_cases,
            ..
        } => (
            run_id,
            sequence,
            json!({"type":"suite_started", "total":total_cases,
                "commit":astra_core::build_info::current().git_sha}),
        ),
        DashboardEvent::CaseQueued {
            run_id,
            sequence,
            case_name,
            model,
            run_index,
            queue_position,
        } => (
            run_id,
            sequence,
            json!({"type":"case_queued", "case":case_name,
                "model":model, "run_index":run_index, "queue_position":queue_position}),
        ),
        DashboardEvent::CaseStarted {
            run_id,
            sequence,
            case_name,
            model,
            run_index,
        } => (
            run_id,
            sequence,
            json!({"type":"case_started", "case":case_name,
                "model":model, "run_index":run_index}),
        ),
        DashboardEvent::CaseProgress {
            run_id,
            sequence,
            case_name,
            model,
            run_index,
            phase,
            elapsed_ms,
        } => (
            run_id,
            sequence,
            json!({"type":"case_progress", "case":case_name,
                "model":model, "run_index":run_index, "phase":phase, "elapsed_ms":elapsed_ms}),
        ),
        DashboardEvent::CaseCompleted {
            run_id,
            sequence,
            report,
        } => (
            run_id,
            sequence,
            json!({"type":"case_completed", "row":row(report)?}),
        ),
        DashboardEvent::SuiteCompleted {
            run_id,
            sequence,
            report,
        } => (
            run_id,
            sequence,
            json!({"type":"suite_completed", "total":report.total(),
                "passed":report.passed(), "failed":report.failed(), "cancelled":report.cancelled(),
                "unavailable":report.unavailable(), "wall_time_ms":report.wall_time_ms}),
        ),
        _ => return Err(io::Error::other("unexpected suite progress event")),
    };
    value["schema"] = json!(SCHEMA);
    value["run_id"] = json!(run);
    value["sequence"] = json!(seq);
    Ok(value)
}

fn write_record(writer: &mut impl Write, value: &Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    if bytes.len() + 1 > MAX_RECORD_BYTES {
        return Err(io::Error::other(
            "suite progress record exceeds byte budget",
        ));
    }
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    writer.flush()
}

/// Loss or unavailable observation cancels through the runner's existing source.
/// Each newline is flushed before the next event; a killed writer leaves a prefix.
pub async fn observe(
    mut writer: impl Write + Send,
    mut receiver: broadcast::Receiver<DashboardEvent>,
    cancel: Arc<AtomicBool>,
) -> io::Result<()> {
    let result = async {
        loop {
            let event = receiver
                .recv()
                .await
                .map_err(|_| io::Error::other("suite progress stream lost"))?;
            write_record(&mut writer, &project(&event)?)?;
            if matches!(event, DashboardEvent::SuiteCompleted { .. }) {
                return Ok(());
            }
        }
    }
    .await;
    if result.is_err() {
        cancel.store(true, Ordering::SeqCst);
        let _ = write_record(
            &mut writer,
            &json!({"schema":SCHEMA,
            "type":"observation_error", "reason":"progress_unavailable"}),
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{report::StepResult, runner::RunOutcome};

    #[test]
    fn canonical_predicate_failure_reports_counts_without_publishing_documents() {
        let mut capture =
            serde_json::to_value(crate::execution_capture::tests::tool_capture()).unwrap();
        capture["transcript"]["items"][1]["tool_result"]["status"] = json!("failed");
        capture["transcript"]["items"][1]["content"] = json!(
            r#"{"error_kind":"tool_invalid_args","status":"rejected","secret":"private-sentinel"}"#
        );
        let mut outcome = RunOutcome::new("model");
        outcome.run_id = Some("root".into());
        outcome.session_id = Some("session".into());
        let mut stream = crate::runner::StreamCapture::default();
        stream.identity_verified = true;
        stream.execution = Some(serde_json::from_value(capture).unwrap());
        outcome.stream_capture = Some(stream);
        let criterion: Criterion = serde_json::from_value(json!({
            "type":"execution_tool_count", "name":"agent", "min":1,"max":1,"root_only":true,"ok":false,
            "predicates":[
                {"document":"arguments","path":"/action","equals":"spawn"},
                {"document":"result","path":"/error_kind","equals":"invalid_request"},
                {"document":"result","path":"/status","equals":"failed"}
            ]
        })).unwrap();
        let results = crate::criteria::evaluate_deterministic(&[criterion], &outcome);
        let projected = criteria(&results, &outcome).unwrap();
        assert_eq!(projected[0]["passed"], false);
        assert_eq!(
            projected[0]["diagnostic"],
            json!({
                "kind":"tool_count", "min":1,"max":1,"actual":0,"considered":1,
                "expected_ok":false,"outcome_matches":1,"error_matches":null,"predicate_matches":[1,0,0]
            })
        );
        assert!(
            !serde_json::to_string(&projected)
                .unwrap()
                .contains("private-sentinel")
        );
        outcome.stream_capture = None;
        assert_eq!(
            criteria(&results, &outcome).unwrap()[0]["diagnostic"]["kind"],
            "unavailable"
        );

        outcome.duration_ms = 80_000;
        let composite = Criterion::AllOf {
            criteria: vec![
                Criterion::ExitCode {
                    code: outcome.exit_code,
                },
                Criterion::DurationBetween {
                    min_ms: 0,
                    max_ms: 45_000,
                },
            ],
        };
        let results = crate::criteria::evaluate_deterministic(&[composite], &outcome);
        let nested = criteria(&results, &outcome).unwrap();
        assert_eq!(
            nested[0]["diagnostic"]["checks"][1]["diagnostic"],
            json!({"kind":"count", "min":0,"max":45_000,"actual":80_000})
        );
    }

    #[tokio::test]
    async fn judge_summary_distinguishes_rejection_from_evaluation_failure() {
        struct Judge(bool);
        #[async_trait::async_trait]
        impl crate::judger::Judger for Judge {
            async fn judge(
                &self,
                _: &Criterion,
                _: &RunOutcome,
                _: Option<&crate::judger::SemanticJudgerEvidence>,
            ) -> Result<crate::judger::JudgerResult, String> {
                if self.0 {
                    return Err("private-sentinel".into());
                }
                Ok(crate::judger::JudgerResult {
                    assessment: crate::judger::JudgerAssessment::Acceptance(false),
                    rationale: "private-sentinel".into(),
                    full_rationale: "private-sentinel".into(),
                    votes: vec![],
                })
            }
        }
        let outcome = RunOutcome::new("model");
        let criterion = Criterion::HardJudger {
            question: "private-sentinel".into(),
            threshold: 0.8,
            model: None,
        };
        for (unavailable, expected) in [(false, "rejected"), (true, "unavailable")] {
            let result =
                crate::judger::evaluate_judger(&Judge(unavailable), &criterion, &outcome, None)
                    .await
                    .unwrap();
            let projected = criteria(&[result], &outcome).unwrap();
            assert_eq!(projected[0]["passed"], false);
            assert_eq!(projected[0]["diagnostic"]["assessment"], expected);
            assert!(
                !serde_json::to_string(&projected)
                    .unwrap()
                    .contains("private-sentinel")
            );
        }
    }

    #[test]
    fn terminal_projection_preserves_grading_and_excludes_every_private_payload() {
        let mut outcome = RunOutcome::new("model");
        outcome.text = "private-sentinel".into();
        outcome.stderr = "private-sentinel".into();
        let mut report: CaseRunReport = serde_json::from_value(json!({
            "case_name":"case", "model":"model", "status":"failed", "outcome":outcome,
            "criteria":[], "weight":1.0
        }))
        .unwrap();
        let checks = [
            (
                Criterion::Judger {
                    question: "private-sentinel".into(),
                    threshold: 0.5,
                    model: None,
                },
                None,
                true,
            ),
            (
                Criterion::HardJudger {
                    question: "private-sentinel".into(),
                    threshold: 1.0,
                    model: None,
                },
                None,
                true,
            ),
            (
                Criterion::HardJudger {
                    question: "private-sentinel".into(),
                    threshold: 1.0,
                    model: None,
                },
                Some(0.0),
                false,
            ),
        ]
        .into_iter()
        .map(|(criterion, score, passed)| CriterionResult {
            severity: crate::criteria::criterion_severity(&criterion),
            criterion,
            passed,
            score,
            detail: "private-sentinel".into(),
            full_detail: Some("private-sentinel".into()),
        })
        .collect::<Vec<_>>();
        report.criteria = checks.clone();
        report.failure_class = Some(crate::classify::FailureClass::InfraProviderError {
            provider: "private-sentinel".into(),
        });
        report.steps = vec![StepResult {
            step_index: 0,
            prompt: "private-sentinel".into(),
            outcome: report.outcome.clone(),
            duration_ms: 20,
            criteria: checks,
            passed: false,
        }];
        let value = row(&report).unwrap();
        assert!(!value.to_string().contains("private-sentinel"));
        assert!(value["criteria"][0]["passed"].is_null());
        assert_eq!(value["criteria"][0]["evaluation"], "unscored");
        assert_eq!(value["criteria"][1]["passed"], true);
        assert_eq!(value["criteria"][1]["evaluation"], "reported");
        assert_eq!(value["steps"][0]["criteria"][2]["score"], 0.0);
        assert_eq!(value["failure_class"], "InfraProviderError");
        assert!(value["token_usage_coverage"].is_null());
        report.criteria[0].score = Some(f64::NAN);
        assert!(row(&report).is_err());
    }

    #[tokio::test]
    async fn observer_failure_or_loss_cancels_instead_of_certifying_completion() {
        struct Fails(bool);
        impl Write for Fails {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                if self.0 {
                    Ok(b.len())
                } else {
                    Err(io::Error::other("test write failure"))
                }
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other("test flush failure"))
            }
        }
        let started = || DashboardEvent::SuiteStarted {
            run_id: "cli".into(),
            total_cases: 1,
            models: vec!["model".into()],
            started_at: "now".into(),
            source: "suite".into(),
            sequence: 1,
        };
        for flush in [false, true] {
            let (tx, rx) = broadcast::channel(2);
            tx.send(started()).unwrap();
            let cancel = Arc::new(AtomicBool::new(false));
            assert!(observe(Fails(flush), rx, cancel.clone()).await.is_err());
            assert!(cancel.load(Ordering::SeqCst));
        }
        for lag in [false, true] {
            let (tx, rx) = broadcast::channel(1);
            tx.send(started()).unwrap();
            if lag {
                tx.send(started()).unwrap();
            }
            drop(tx);
            let cancel = Arc::new(AtomicBool::new(false));
            let mut bytes = Vec::new();
            assert!(observe(&mut bytes, rx, cancel.clone()).await.is_err());
            assert!(cancel.load(Ordering::SeqCst));
            assert!(
                String::from_utf8(bytes)
                    .unwrap()
                    .contains("observation_error")
            );
        }
        assert!(write_record(&mut Vec::new(), &json!("x".repeat(MAX_RECORD_BYTES))).is_err());
    }
}
