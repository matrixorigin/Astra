//! Secret-free, incremental projection of the existing suite event stream.
use crate::{
    criteria::{Criterion, CriterionResult},
    dashboard::DashboardEvent,
    report::CaseRunReport,
};
use serde_json::{Value, json};
use std::io::{self, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::broadcast;

pub const SCHEMA: &str = "astra.harness_progress.v1";
pub const MAX_RECORD_BYTES: usize = 262_144;

fn criteria(values: &[CriterionResult]) -> io::Result<Vec<Value>> {
    values
        .iter()
        .map(|c| {
            if c.score.is_some_and(|s| !s.is_finite()) {
                return Err(io::Error::other("invalid progress score"));
            }
            let tag = serde_json::to_value(&c.criterion)?["type"].clone();
            let advisory = matches!(c.criterion, Criterion::Judger { .. });
            let judge = advisory || matches!(c.criterion, Criterion::HardJudger { .. });
            Ok(json!({"type":tag, "severity":c.severity, "score":c.score,
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
                "criteria":criteria(&s.criteria)?
            }))
        })
        .collect();
    Ok(
        json!({"case":report.case_name, "model":report.model, "run_index":report.run_index,
        "status":report.status, "has_warnings":report.has_warnings,
        "failure_class":failure, "exit_code":o.exit_code,
        "criteria":criteria(&report.criteria)?, "steps":steps?,
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
