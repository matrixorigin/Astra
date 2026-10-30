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

pub const SCHEMA: &str = "astra.harness_progress.v8";
pub const MAX_RECORD_BYTES: usize = 262_144;

// Closed projections only: even machine-looking upstream tags can contain
// private input. Never publish stderr, arbitrary codes, or live event bodies.
fn error_kind(value: &str) -> &'static str {
    match value {
        // CLI terminal categories are distinct from provider ErrorKind tags.
        "api_error" => "api_error",
        "tool_failure" => "tool_failure",
        "persistence_error" => "persistence_error",
        "partial" => "partial",
        "unfinished" => "unfinished",
        "session_cancellation_pending" => "session_cancellation_pending",
        _ => serde_json::from_value::<astra_core::ErrorKind>(json!(value))
            .map_or("other", astra_core::ErrorKind::as_str),
    }
}

fn interruption(value: &str) -> &'static str {
    match value {
        // Synthetic outer harness outcomes, not runtime interruption kinds.
        "timeout" => "timeout",
        "cancelled" => "cancelled",
        _ => astra_turn_core::interruption::InterruptionKind::from_label(value)
            .map_or("other", |kind| kind.label()),
    }
}

fn error_code(value: &str) -> &'static str {
    match value {
        "canonical_persistence_failed" => "canonical_persistence_failed",
        "crash_recovery" => "crash_recovery",
        "genesis_unavailable" => "genesis_unavailable",
        "terminal_handoff_window_closed" => "terminal_handoff_window_closed",
        "internal_error" => "internal_error",
        "tool_error" => "tool_error",
        "api_error" => "api_error",
        _ => {
            let kind = error_kind(value);
            if kind != "other" {
                kind
            } else {
                interruption(value)
            }
        }
    }
}

fn loop_finish_reason(value: Option<&str>) -> &str {
    match value {
        Some(
            code @ ("stop"
            | "content_filter"
            | "function_call"
            | "length"
            | "tool_calls"
            | "continue"
            | "interrupted"
            | "superseded_by_runtime_input"
            | "direct_child_completion_wait"
            | "runtime_input_arrived_before_settlement"
            | "textless_response_retry"
            | "canonical_work_revalidation_unavailable"
            | "paused_after_provider_response"
            | "canonical_work_synthesis_revoked"
            | "work_settlement_retry"
            | "completion_action_no_tool_retry"
            | "completion_action_incomplete"
            | "workspace_completion_retry"
            | "explicit_verification_retry"
            | "round_slice_without_typed_completion"
            | "completion_contract_incomplete"
            | "token_budget_exceeded"),
        ) => code,
        Some(_) => "other",
        None => "not_reported",
    }
}

fn child_boundary_outcome(value: Option<&str>) -> &str {
    match value {
        Some(
            code @ ("results_staged"
            | "results_adopted"
            | "wait_started"
            | "cancelled"
            | "readmission_failed"
            | "synthesis_ready"
            | "runtime_input_ready"
            | "observation_timed_out"
            | "synthesis_budget_exhausted"
            | "deadline"
            | "finalization_accepted"
            | "finalization_incomplete"
            | "finalization_interrupted"),
        ) => code,
        Some(_) => "other",
        None => "not_reported",
    }
}

fn execution_diagnostic(
    outcome: &RunOutcome,
    session: Option<&crate::session_capture::SessionCapture>,
) -> Value {
    use crate::runner::StreamCaptureRecord;
    use astra_turn_core::agent_live_event::{AgentLiveEventKind, AgentLiveSignal};
    use std::collections::BTreeMap;

    let stream = outcome.stream_capture.as_ref().map(|stream| {
        let mut counts = BTreeMap::<&str, u64>::new();
        let mut last_event = None;
        for record in &stream.records {
            let tag = match record {
                StreamCaptureRecord::AgentLiveGap { .. } => "gap",
                StreamCaptureRecord::AgentLive { event, .. } => match &event.kind {
                    AgentLiveEventKind::OutputDelta { .. } => "output_delta",
                    AgentLiveEventKind::AgentTerminated { termination, .. } => {
                        use astra_turn_core::agent_live_event::AgentLiveTermination;
                        match termination {
                            AgentLiveTermination::Completed => "terminated_completed",
                            AgentLiveTermination::Delegated => "terminated_delegated",
                            AgentLiveTermination::Failed => "terminated_failed",
                            AgentLiveTermination::Interrupted => "terminated_interrupted",
                            AgentLiveTermination::Cancelled => "terminated_cancelled",
                        }
                    }
                    AgentLiveEventKind::Signal(AgentLiveSignal::RunStarted { .. }) => "run_started",
                    AgentLiveEventKind::Signal(AgentLiveSignal::TranscriptCommitted { .. }) => {
                        "transcript_committed"
                    }
                    _ => "other",
                },
            };
            *counts.entry(tag).or_default() += 1;
            last_event = Some(tag);
        }
        // Event counts are observations, not a certified count/status of children.
        json!({"identity_verified":stream.identity_verified, "has_diagnostics":!stream.diagnostics.is_empty(),
            "last_event":last_event, "event_counts":counts})
    });
    let journal = session.map(|session| {
        let mut counts = BTreeMap::<&str, u64>::new();
        let mut last_event = None;
        let mut last_provider_failure = None::<(chrono::DateTime<chrono::Utc>, Value)>;
        let mut failure_order_unknown = session.skipped_lines > 0
            || session.dropped_lines > 0
            || session.has_integrity_errors();
        let mut latest_failure_conflicts = false;
        let mut loop_reasons = BTreeMap::<&str, BTreeMap<&str, u64>>::new();
        let mut child_boundaries = BTreeMap::<&str, BTreeMap<&str, u64>>::new();
        for event in &session.events {
            let logical = crate::session_capture::logical_event(&event.raw);
            let run = logical.pointer("/producer_scope/run_id").and_then(Value::as_str)
                .or_else(|| logical["run_id"].as_str());
            let scope = match (run, outcome.run_id.as_deref()) {
                (Some(run), Some(root)) if run == root => "root",
                (Some(_), Some(_)) => "other_run",
                _ => "unattributed",
            };
            if event.event_type == "llm_round" {
                let code = loop_finish_reason(logical.pointer("/metadata/finish_reason").and_then(Value::as_str));
                *loop_reasons.entry(scope).or_default().entry(code).or_default() += 1;
            }
            if event.event_type == "trace_span" && logical.pointer("/metadata/name")
                .and_then(Value::as_str) == Some("agent_dependency_boundary") {
                let code = child_boundary_outcome(logical.pointer("/metadata/attrs/outcome").and_then(Value::as_str));
                *child_boundaries.entry(scope).or_default().entry(code).or_default() += 1;
            }
            let tag = match event.event_type.as_str() {
                "llm_round" => "llm_round",
                "turn" => "turn",
                "turn_error" => "turn_error",
                "tool_invocation" => "tool_invocation",
                "LlmRoundStarted" => "llm_round_started",
                "LlmRoundCompleted" => "llm_round_completed",
                "LlmRoundFailed" => "llm_round_failed",
                "ToolCallStarted" => "tool_call_started",
                "ToolCallCompleted" => "tool_call_completed",
                "ToolCallFailed" => "tool_call_failed",
                "ToolCallSkipped" => "tool_call_skipped",
                "StepStarted" => "step_started",
                "StepCompleted" => "step_completed",
                "StepIncomplete" => "step_incomplete",
                "StepFailed" => "step_failed",
                _ => "other",
            };
            *counts.entry(tag).or_default() += 1;
            last_event = Some(tag);
            if matches!(tag, "llm_round_failed" | "llm_round_completed") {
                let logical = crate::session_capture::logical_event(&event.raw);
                let payload = &logical["payload"];
                if tag == "llm_round_failed"
                    || payload.get("error_kind").is_some_and(|kind| !kind.is_null())
                {
                    let scope = match (logical["run_id"].as_str(), outcome.run_id.as_deref()) {
                        (Some(run), Some(root)) if run == root => "root",
                        (Some(_), Some(_)) => "other_run",
                        _ => "unattributed",
                    };
                    let phase = payload["deadline_phase"].as_str().map(|phase| match phase {
                        "semantic_progress" => "semantic_progress",
                        "actionable_output" => "actionable_output",
                        "provider_work" => "provider_work",
                        "consuming the provider stream" => "consuming_provider_stream",
                        "waiting for provider response" => "waiting_provider_response",
                        "sending the non-stream provider request" => "sending_provider_request",
                        "provider_attempt_admission" => "provider_attempt_admission",
                        "provider_attempt_terminalization" => "provider_attempt_terminalization",
                        "provider_attempt_persistence" => "provider_attempt_persistence",
                        _ => "other",
                    });
                    let failure = json!({
                        "event":tag,
                        "run_scope":scope,
                        "error_kind":payload["error_kind"].as_str().map(error_kind),
                        "failure_stage":payload["failure_stage"].as_str().map(|stage| match stage {
                            "admission" => "admission", "provider" => "provider", "settlement" => "settlement",
                            _ => "other",
                        }),
                        "deadline_phase":phase,
                        "dispatch_started":payload["dispatch_started"].as_bool(),
                        "provider_completed":payload["provider_completed"].as_bool(),
                    });
                    // Owner-artifact merge order is not chronological. Compare
                    // producer time; missing time or conflicting projections
                    // at the latest timestamp cannot identify a latest failure.
                    if let Some(time) = crate::session_capture::event_timestamp(&event.raw) {
                        match &last_provider_failure {
                            Some((latest, previous)) if time == *latest => {
                                latest_failure_conflicts |= failure != *previous;
                            }
                            Some((latest, _)) if time < *latest => {}
                            _ => {
                                last_provider_failure = Some((time, failure));
                                latest_failure_conflicts = false;
                            }
                        }
                    } else {
                        failure_order_unknown = true;
                    }
                }
            }
        }
        let last_provider_failure = last_provider_failure.and_then(|(_, failure)| {
            (!failure_order_unknown && !latest_failure_conflicts).then_some(failure)
        });
        json!({"skipped_lines":session.skipped_lines, "dropped_lines":session.dropped_lines,
            "integrity_errors":session.integrity_errors,
            "last_event":last_event, "event_counts":counts,
            "last_provider_failure":last_provider_failure,
            "loop_boundaries":{"llm_round":loop_reasons, "child_barrier":child_boundaries}})
    });
    json!({
        "final_state":outcome.final_state.as_deref().map(|state| match state {
            "completed" => "completed", "interrupted" => "interrupted", "failed" => "failed",
            _ => "other",
        }),
        "interruption_kind":outcome.interruption_kind.as_deref().map(interruption),
        "error_kind":outcome.error_kind.as_deref().map(error_kind),
        "error_code":outcome.error_code.as_deref().map(error_code),
        "server_admission_not_dispatched":outcome.server_admission_not_dispatched,
        "stream":stream, "journal":journal,
    })
}

fn criteria(
    values: &[CriterionResult],
    outcome: &RunOutcome,
    session: Option<&crate::session_capture::SessionCapture>,
) -> io::Result<Vec<Value>> {
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
                    crate::criteria::execution_contract_proof(&c.criterion, outcome, session)
                        .map(|(_, diagnostic)| diagnostic)
                        .unwrap_or_else(|failure| failure.diagnostic()),
                Criterion::JournalToolCallCount { .. }
                | Criterion::JournalToolSuccessRatio { .. } => {
                    match c.full_detail.as_deref().and_then(|raw| {
                        serde_json::from_str::<crate::criteria::JournalDiagnostic>(raw).ok()
                    }) {
                        Some(diagnostic) => serde_json::to_value(diagnostic)?,
                        None => Value::Null,
                    }
                }
                Criterion::JournalToolOutcomeCount { name, ok, min, max } => session
                    .filter(|capture| capture.skipped_lines == 0 && capture.dropped_lines == 0
                        && !capture.has_integrity_errors())
                    .map(|session| json!({"kind":"count", "min":min, "max":max,
                        "actual":session.journal_tool_outcome_count(name, *ok)}))
                    .unwrap_or(Value::Null),
                Criterion::DurationBetween { min_ms, max_ms } =>
                    json!({"kind":"count", "min":min_ms, "max":max_ms, "actual":outcome.duration_ms}),
                Criterion::ToolsCountBetween { min, max } =>
                    json!({"kind":"count", "min":min, "max":max, "actual":outcome.observed_tool_calls()}),
                Criterion::TurnRoundsBetween { min, max } =>
                    json!({"kind":"count", "min":min, "max":max, "actual":outcome.turn_rounds}),
                Criterion::ExitCode { code } =>
                    json!({"kind":"exit", "expected":code, "actual":outcome.exit_code}),
                Criterion::Judger { threshold, .. } | Criterion::HardJudger { threshold, .. } => {
                    let (assessment, failure) = crate::judger::assessment_summary(c);
                    json!({"kind":"judge", "threshold":threshold, "assessment":assessment, "failure":failure})
                }
                Criterion::PromptCacheReuseScope { .. } => {
                    let diagnostic = c.full_detail.as_deref()
                        .and_then(|raw| serde_json::from_str::<crate::criteria::CacheScopeDiagnostic>(raw).ok());
                    match diagnostic {
                        Some(diagnostic) => {
                            let mut value = diagnostic.safe_value()?;
                            value["kind"] = json!("cache_scope");
                            value
                        }
                        None => Value::Null,
                    }
                }
                Criterion::AllOf { .. } | Criterion::AnyOf { .. } => {
                    let nested = c.full_detail.as_deref()
                        .and_then(|raw| serde_json::from_str::<Vec<CriterionResult>>(raw).ok());
                    match nested {
                        Some(nested) => json!({"kind":"composite", "checks":criteria(&nested, outcome, session)?}),
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

fn step_journal(
    report: &CaseRunReport,
    outcome: &RunOutcome,
) -> Option<crate::session_capture::SessionCapture> {
    let session_id = outcome
        .session_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())?;
    let run_id = outcome
        .run_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())?;
    report
        .session
        .iter()
        .chain(&report.session_captures)
        .filter(|capture| capture.session_id == session_id)
        .find_map(|capture| {
            // Run identity is immutable and unique. The report already owns
            // its captured time window; reuse the canonical causal scoper
            // without inventing an invocation start time at projection time.
            let scoped = capture.scoped_to_invocation(
                &[run_id.to_string()],
                chrono::DateTime::<chrono::Utc>::MIN_UTC,
            );
            scoped
                .events
                .iter()
                .any(|event| {
                    crate::session_capture::event_invocation_binding(&event.raw)
                        .is_some_and(|(run, _)| run == run_id)
                })
                .then_some(scoped)
        })
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
            let session = step_journal(report, &s.outcome);
            Ok(json!({
                "step_index":s.step_index, "passed":s.passed, "duration_ms":s.duration_ms,
                "execution_diagnostic":execution_diagnostic(&s.outcome, session.as_ref()),
                "criteria":criteria(&s.criteria, &s.outcome, session.as_ref())?
            }))
        })
        .collect();
    Ok(
        json!({"case":report.case_name, "model":report.model, "run_index":report.run_index,
        "status":report.status, "has_warnings":report.has_warnings,
        "failure_class":failure, "exit_code":o.exit_code,
        "execution_diagnostic":execution_diagnostic(o, report.session.as_ref()),
        "criteria":criteria(&report.criteria, o, report.session.as_ref())?, "steps":steps?,
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
                "commit":astra_build_info::current().git_sha}),
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
    fn loop_boundaries_preserve_scoped_reasons_without_private_trace_attributes() {
        use crate::session_capture::{JournalEvent, SessionCapture};
        let mut events = Vec::new();
        for (run, reason) in [
            (Some("root-private-sentinel"), "textless_response_retry"),
            (Some("root-private-sentinel"), "stop"),
            (Some("child-private-sentinel"), "tool_calls"),
            (None, "private-sentinel"),
        ] {
            events.push(JournalEvent {
                event_type: "llm_round".into(),
                raw: json!({
                    "type":"llm_round", "producer_scope":{"run_id":run},
                    "metadata":{"finish_reason":reason, "private-sentinel":"private-sentinel"}
                }),
            });
        }
        let attrs = std::collections::HashMap::from([
            ("outcome".into(), "results_adopted".into()),
            ("children".into(), "private-sentinel".into()),
        ]);
        let trace = astra_services::session_journal::JournalEvent::trace_span(
            Some("private-sentinel"),
            Some(1),
            "private-sentinel",
            None,
            "agent_dependency_boundary",
            1,
            2,
            Some(&attrs),
            Some("private-sentinel"),
        )
        .with_producer_scope(Some("root-private-sentinel"));
        events.push(JournalEvent {
            event_type: "trace_span".into(),
            raw: serde_json::to_value(trace).unwrap(),
        });
        let session = SessionCapture {
            events,
            ..Default::default()
        };
        let outcome = RunOutcome {
            run_id: Some("root-private-sentinel".into()),
            ..RunOutcome::new("model")
        };
        let diagnostic = execution_diagnostic(&outcome, Some(&session));
        let boundaries = &diagnostic["journal"]["loop_boundaries"];
        assert_eq!(
            boundaries["llm_round"]["root"]["textless_response_retry"],
            1
        );
        assert_eq!(boundaries["llm_round"]["root"]["stop"], 1);
        assert_eq!(boundaries["llm_round"]["other_run"]["tool_calls"], 1);
        assert_eq!(boundaries["llm_round"]["unattributed"]["other"], 1);
        assert_eq!(boundaries["child_barrier"]["root"]["results_adopted"], 1);
        assert!(!diagnostic.to_string().contains("private-sentinel"));
    }

    #[test]
    fn execution_diagnostics_keep_typed_failure_and_missing_evidence_distinct() {
        let outcome = RunOutcome {
            exit_code: 3,
            final_state: Some("failed".into()),
            error_kind: Some("contract_violation".into()),
            error_code: Some("canonical_persistence_failed".into()),
            text: "private-sentinel".into(),
            stderr: "private-sentinel".into(),
            ..RunOutcome::new("model")
        };
        let diagnostic = execution_diagnostic(&outcome, None);
        assert_eq!(diagnostic["final_state"], "failed");
        assert_eq!(diagnostic["error_kind"], "contract_violation");
        assert_eq!(diagnostic["error_code"], "canonical_persistence_failed");
        assert!(diagnostic["stream"].is_null());
        assert!(diagnostic["journal"].is_null());
        assert!(!diagnostic.to_string().contains("private-sentinel"));
        for kind in ["timeout", "stream_transport", "executor_dropped"] {
            let timeout = RunOutcome {
                final_state: Some("interrupted".into()),
                interruption_kind: Some(kind.into()),
                ..outcome.clone()
            };
            assert_eq!(
                execution_diagnostic(&timeout, None)["interruption_kind"],
                kind
            );
        }
    }

    #[test]
    fn execution_diagnostics_redact_unknown_tags_and_count_only_event_shapes() {
        use crate::session_capture::{JournalEvent, SessionCapture};
        let mut stream = crate::runner::StreamCapture::default();
        for (index, kind) in [
            json!({"type":"output_delta", "model_item_id":null, "text":"private-sentinel"}),
            json!({"type":"agent_terminated", "termination":"completed", "duration_ms":20,
                "reason":"private-sentinel"}),
        ]
        .into_iter()
        .enumerate()
        {
            stream.observe(
                &json!({"type":"agent_live", "event":{
                    "run_id":"private-sentinel", "agent_id":"private-sentinel", "kind":kind
                }}),
                index as u64,
            );
        }
        let outcome = RunOutcome {
            final_state: Some("private-sentinel".into()),
            error_kind: Some("private-sentinel".into()),
            error_code: Some("private-sentinel".into()),
            interruption_kind: Some("private-sentinel".into()),
            stream_capture: Some(stream),
            ..RunOutcome::new("model")
        };
        let session = SessionCapture {
            events: [
                "llm_round",
                "ToolCallCompleted",
                "turn_error",
                "private-sentinel",
            ]
            .into_iter()
            .map(|tag| JournalEvent {
                event_type: tag.into(),
                raw: json!({"private-sentinel":"private-sentinel"}),
            })
            .collect(),
            dropped_lines: 2,
            ..Default::default()
        };
        let diagnostic = execution_diagnostic(&outcome, Some(&session));
        for field in [
            "final_state",
            "error_kind",
            "error_code",
            "interruption_kind",
        ] {
            assert_eq!(diagnostic[field], "other");
        }
        assert_eq!(diagnostic["stream"]["identity_verified"], false);
        assert_eq!(diagnostic["stream"]["has_diagnostics"], false);
        assert_eq!(diagnostic["stream"]["last_event"], "terminated_completed");
        assert_eq!(
            diagnostic["stream"]["event_counts"]["terminated_completed"],
            1
        );
        assert_eq!(diagnostic["stream"]["event_counts"]["output_delta"], 1);
        assert_eq!(diagnostic["journal"]["event_counts"]["turn_error"], 1);
        assert_eq!(
            diagnostic["journal"]["event_counts"]["tool_call_completed"],
            1
        );
        assert_eq!(diagnostic["journal"]["last_event"], "other");
        assert_eq!(diagnostic["journal"]["dropped_lines"], 2);
        assert!(!diagnostic.to_string().contains("private-sentinel"));
    }

    #[test]
    fn provider_failure_projection_uses_existing_logical_event_and_closed_tags() {
        use crate::session_capture::{JournalEvent, SessionCapture};
        let outcome = RunOutcome {
            run_id: Some("root".into()),
            ..RunOutcome::new("model")
        };
        for (wrapped, kind, stage, phase, expected_scope) in [
            (
                false,
                "provider_deadline",
                "provider",
                "waiting for provider response",
                "root",
            ),
            (
                true,
                "contract_violation",
                "settlement",
                "provider_attempt_persistence",
                "other_run",
            ),
            (
                true,
                "private-sentinel",
                "private-sentinel",
                "private-sentinel",
                "other_run",
            ),
        ] {
            let logical = json!({"run_id":if wrapped {"private-sentinel"} else {"root"}, "created_at":300,
            "event_type":"LlmRoundFailed", "payload":{
                "error_kind":kind, "failure_stage":stage, "deadline_phase":phase,
                "dispatch_started":true, "provider_completed":false, "message":"private-sentinel"
            }});
            let session = SessionCapture {
                events: vec![JournalEvent {
                    event_type: "LlmRoundFailed".into(),
                    raw: if wrapped {
                        json!({"artifact_kind":"step_event", "payload":logical})
                    } else {
                        logical
                    },
                }],
                ..Default::default()
            };
            let diagnostic = execution_diagnostic(&outcome, Some(&session));
            let failure = &diagnostic["journal"]["last_provider_failure"];
            assert_eq!(failure["run_scope"], expected_scope);
            assert_eq!(
                failure["error_kind"],
                if kind == "private-sentinel" {
                    "other"
                } else {
                    kind
                }
            );
            assert_eq!(
                failure["deadline_phase"],
                match phase {
                    "waiting for provider response" => "waiting_provider_response",
                    "provider_attempt_persistence" => "provider_attempt_persistence",
                    _ => "other",
                }
            );
            assert_eq!(failure["dispatch_started"], true);
            assert!(!diagnostic.to_string().contains("private-sentinel"));
        }
    }

    fn failure_event(
        run: &str,
        kind: &str,
        created_at: Value,
    ) -> crate::session_capture::JournalEvent {
        crate::session_capture::JournalEvent {
            event_type: "LlmRoundFailed".into(),
            raw: json!({"event_type":"LlmRoundFailed", "run_id":run, "created_at":created_at,
                "payload":{"error_kind":kind, "failure_stage":"provider", "message":"private-sentinel"}}),
        }
    }

    fn diagnostic_report() -> CaseRunReport {
        serde_json::from_value(json!({
            "case_name":"multi-step", "model":"model", "status":"failed",
            "outcome":RunOutcome {
                session_id:Some("session".into()), run_id:Some("root".into()),
                ..RunOutcome::new("model")
            }, "criteria":[]
        }))
        .unwrap()
    }

    #[test]
    fn multi_step_diagnostics_bind_each_journal_to_its_invocation() {
        use crate::{
            report::StepResult,
            session_capture::{JournalEvent, SessionCapture},
        };
        let mut report = diagnostic_report();
        report.session = Some(SessionCapture {
            session_id: "session".into(),
            events: vec![
                failure_event("step", "provider_deadline", json!(200)),
                failure_event("root", "auth", json!(300)),
                JournalEvent {
                    event_type: "agent_spawned".into(),
                    raw: json!({
                        "type":"agent_spawned", "ts":"1970-01-01T00:00:00.210Z",
                        "metadata":{"parent_run_id":"step", "run_id":"child"}
                    }),
                },
                JournalEvent {
                    event_type: "ToolCallCompleted".into(),
                    raw: json!({
                        "event_type":"ToolCallCompleted", "run_id":"child", "created_at":220
                    }),
                },
                JournalEvent {
                    event_type: "llm_round".into(),
                    raw: json!({
                        "type":"llm_round", "ts":"1970-01-01T00:00:00.400Z",
                        "producer_scope":{"run_id":"next-step"}, "metadata":{"finish_reason":"stop"}
                    }),
                },
            ],
            ..Default::default()
        });
        for (step_index, run) in [(0, "step"), (1, "next-step")] {
            report.steps.push(StepResult {
                step_index,
                prompt: "private-sentinel".into(),
                duration_ms: 1,
                outcome: RunOutcome {
                    session_id: Some("session".into()),
                    run_id: Some(run.into()),
                    ..RunOutcome::new("model")
                },
                criteria: vec![],
                passed: step_index != 0,
            });
        }
        let projected = row(&report).unwrap();
        assert_eq!(
            projected["execution_diagnostic"]["journal"]["last_provider_failure"]["error_kind"],
            "auth"
        );
        let first = &projected["steps"][0]["execution_diagnostic"]["journal"];
        assert_eq!(
            first["last_provider_failure"]["error_kind"],
            "provider_deadline"
        );
        assert_eq!(first["last_provider_failure"]["run_scope"], "root");
        assert_eq!(first["event_counts"]["llm_round_failed"], 1);
        assert_eq!(first["event_counts"]["tool_call_completed"], 1);
        let second = &projected["steps"][1]["execution_diagnostic"]["journal"];
        assert_eq!(second["event_counts"]["llm_round"], 1);
        assert!(second["last_provider_failure"].is_null());
        assert!(second["event_counts"].get("llm_round_failed").is_none());
        assert!(!projected.to_string().contains("private-sentinel"));
    }

    #[test]
    fn step_diagnostic_requires_matching_session_and_typed_run_evidence() {
        use crate::{report::StepResult, session_capture::SessionCapture};
        let mut report = diagnostic_report();
        let capture = SessionCapture {
            session_id: "session".into(),
            events: vec![failure_event("step", "provider_deadline", json!(200))],
            ..Default::default()
        };
        report.session_captures.push(capture.clone());
        report.steps.push(StepResult {
            step_index: 0,
            prompt: String::new(),
            duration_ms: 1,
            outcome: RunOutcome {
                session_id: Some("session".into()),
                run_id: Some("step".into()),
                ..RunOutcome::new("model")
            },
            criteria: vec![],
            passed: false,
        });
        assert_eq!(
            row(&report).unwrap()["steps"][0]["execution_diagnostic"]["journal"]["event_counts"]["llm_round_failed"],
            1
        );
        for (session, run, timestamp) in [
            (Some("other-session"), Some("step"), json!(200)),
            (None, Some("step"), json!(200)),
            (Some("session"), None, json!(200)),
            (Some("session"), Some(" "), json!(200)),
            (Some("session"), Some("unmatched"), json!(200)),
            (Some("session"), Some("step"), Value::Null),
        ] {
            report.steps[0].outcome.session_id = session.map(str::to_string);
            report.steps[0].outcome.run_id = run.map(str::to_string);
            report.session_captures[0].events[0] =
                failure_event("step", "provider_deadline", timestamp);
            assert!(row(&report).unwrap()["steps"][0]["execution_diagnostic"]["journal"].is_null());
        }
    }

    #[test]
    fn latest_provider_failure_uses_event_time_not_artifact_order() {
        use crate::session_capture::SessionCapture;
        let outcome = RunOutcome {
            run_id: Some("root".into()),
            ..RunOutcome::new("model")
        };
        let mut latest = failure_event("root", "provider_deadline", Value::Null);
        latest.raw["ts"] = json!("1970-01-01T01:00:00.300+01:00");
        let mut older = failure_event("child", "auth", json!(50));
        older.raw = json!({"artifact_kind":"step_event", "payload":older.raw});
        for events in [vec![latest.clone(), older.clone()], vec![older, latest]] {
            let capture = SessionCapture {
                events,
                ..Default::default()
            };
            let diagnostic = execution_diagnostic(&outcome, Some(&capture));
            assert_eq!(
                diagnostic["journal"]["last_provider_failure"]["error_kind"],
                "provider_deadline"
            );
            assert_eq!(
                diagnostic["journal"]["last_provider_failure"]["run_scope"],
                "root"
            );
            assert!(!diagnostic.to_string().contains("private-sentinel"));
        }
    }

    #[test]
    fn latest_provider_failure_is_unknown_without_a_unique_ordered_projection() {
        use crate::session_capture::SessionCapture;
        let outcome = RunOutcome::new("model");
        for events in [
            vec![failure_event("root", "auth", Value::Null)],
            vec![
                failure_event("root", "provider_deadline", json!(300)),
                failure_event("child", "auth", Value::Null),
            ],
            vec![
                failure_event("child", "auth", Value::Null),
                failure_event("root", "provider_deadline", json!(300)),
            ],
            vec![
                failure_event("root", "provider_deadline", json!(300)),
                failure_event("child", "auth", json!(300)),
            ],
        ] {
            let count = events.len();
            let capture = SessionCapture {
                events,
                ..Default::default()
            };
            let diagnostic = execution_diagnostic(&outcome, Some(&capture));
            assert!(diagnostic["journal"]["last_provider_failure"].is_null());
            assert_eq!(
                diagnostic["journal"]["event_counts"]["llm_round_failed"],
                count
            );
        }
        let capture = SessionCapture {
            events: vec![
                failure_event("root", "provider_deadline", json!(300)),
                failure_event("child", "auth", json!(50)),
                failure_event("child", "network", json!(50)),
            ],
            ..Default::default()
        };
        assert_eq!(
            execution_diagnostic(&outcome, Some(&capture))["journal"]["last_provider_failure"]["error_kind"],
            "provider_deadline"
        );
    }

    #[test]
    fn invalid_failure_time_survives_loader_scope_and_progress_as_uncertainty() {
        use crate::session_capture::load_session_from_path;
        use astra_pipeline::step_protocol::{StepEvent, StepEventType};
        for wrapped in [false, true] {
            for target in ["root", "step"] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("step_events.jsonl");
                let events = [
                    ("earlier", 50, "auth"),
                    ("unorderable", u64::MAX, "provider_deadline"),
                ]
                .map(|(id, created_at, kind)| {
                    let event = serde_json::to_value(StepEvent {
                        event_id: id.into(),
                        run_id: target.into(),
                        canonical_event_id: None,
                        step_id: "step-id".into(),
                        event_type: StepEventType::LlmRoundFailed,
                        agent_id: None,
                        caused_by: vec![],
                        created_at,
                        payload: Some(json!({"error_kind":kind, "failure_stage":"provider",
                                "message":"private-sentinel"})),
                    })
                    .unwrap();
                    if wrapped {
                        json!({"artifact_kind":"step_event", "payload":event})
                    } else {
                        event
                    }
                });
                std::fs::write(
                    &path,
                    events
                        .iter()
                        .map(Value::to_string)
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
                .unwrap();
                let capture = load_session_from_path("session", &path).unwrap();
                let mut report = diagnostic_report();
                assert!(execution_diagnostic(&report.outcome, Some(&capture))["journal"]["last_provider_failure"].is_null());
                // Match SuiteRunner's time window followed by row's per-step
                // invocation scope, rather than bypassing those production owners.
                report.session = Some(capture.scoped_to_invocation(
                    &["root".into(), "step".into()],
                    chrono::DateTime::UNIX_EPOCH,
                ));
                report.steps.push(StepResult {
                    step_index: 0,
                    prompt: "private-sentinel".into(),
                    duration_ms: 1,
                    outcome: RunOutcome {
                        session_id: Some("session".into()),
                        run_id: Some("step".into()),
                        ..RunOutcome::new("model")
                    },
                    criteria: vec![],
                    passed: false,
                });
                let projected = row(&report).unwrap();
                let root = &projected["execution_diagnostic"]["journal"];
                assert!(
                    root["last_provider_failure"].is_null(),
                    "{wrapped} {target}: {root}"
                );
                assert!(
                    root["skipped_lines"].as_u64().unwrap()
                        + root["integrity_errors"].as_u64().unwrap()
                        > 0
                );
                if target == "step" {
                    let step = &projected["steps"][0]["execution_diagnostic"]["journal"];
                    assert!(step["last_provider_failure"].is_null());
                    assert_eq!(step["event_counts"]["llm_round_failed"], 1);
                    assert!(
                        step["skipped_lines"].as_u64().unwrap()
                            + step["integrity_errors"].as_u64().unwrap()
                            > 0
                    );
                }
                assert!(!projected.to_string().contains("private-sentinel"));
            }
        }
    }

    #[test]
    fn incomplete_capture_never_claims_a_latest_provider_failure() {
        use crate::session_capture::SessionCapture;
        let outcome = RunOutcome::new("model");
        for (skipped_lines, dropped_lines, integrity_errors) in [(1, 0, 0), (0, 1, 0), (0, 0, 1)] {
            let capture = SessionCapture {
                events: vec![failure_event("root", "auth", json!(50))],
                skipped_lines,
                dropped_lines,
                integrity_errors,
                ..Default::default()
            };
            let diagnostic = execution_diagnostic(&outcome, Some(&capture));
            assert!(diagnostic["journal"]["last_provider_failure"].is_null());
            assert_eq!(diagnostic["journal"]["event_counts"]["llm_round_failed"], 1);
        }
    }

    #[test]
    fn failed_event_without_error_kind_is_not_hidden_by_an_older_failure() {
        use crate::session_capture::SessionCapture;
        let outcome = RunOutcome::new("model");
        let mut latest = failure_event("root", "provider_deadline", json!(100));
        latest.raw["payload"] = json!({});
        let mut success = failure_event("root", "auth", json!(200));
        success.event_type = "LlmRoundCompleted".into();
        success.raw["event_type"] = json!("LlmRoundCompleted");
        success.raw["payload"] = json!({"error_kind":null});
        let capture = SessionCapture {
            events: vec![failure_event("root", "auth", json!(50)), latest, success],
            ..Default::default()
        };
        let diagnostic = execution_diagnostic(&outcome, Some(&capture));
        let failure = &diagnostic["journal"]["last_provider_failure"];
        assert_eq!(failure["event"], "llm_round_failed");
        assert!(failure["error_kind"].is_null());
    }

    fn fixture_evaluate(criteria: &[Criterion], outcome: &RunOutcome) -> Vec<CriterionResult> {
        let trace = outcome
            .stream_capture
            .as_ref()
            .and_then(|s| s.execution.as_ref())
            .map(crate::execution_capture::tests::trace);
        crate::criteria::evaluate_deterministic_with_session(criteria, outcome, trace.as_ref())
    }

    fn fixture_criteria(
        values: &[CriterionResult],
        outcome: &RunOutcome,
    ) -> io::Result<Vec<Value>> {
        let trace = outcome
            .stream_capture
            .as_ref()
            .and_then(|s| s.execution.as_ref())
            .map(crate::execution_capture::tests::trace);
        super::criteria(values, outcome, trace.as_ref())
    }

    #[test]
    fn journal_outcome_progress_matches_verdict_without_exposing_tool_payloads() {
        use crate::session_capture::{JournalEvent, SessionCapture};
        let outcome = RunOutcome::new("model");
        let criterion = Criterion::JournalToolOutcomeCount {
            name: "write_file".into(),
            ok: true,
            min: 1,
            max: 1,
        };
        for count in [0, 1, 2] {
            let mut calls: Vec<_> = (0..count)
                .map(|index| {
                    json!({
                        "tool_call_id":format!("call-{index}"), "name":"write_file", "ok":true,
                        "args_full":"private-sentinel", "result_full":"private-sentinel"
                    })
                })
                .collect();
            calls.push(json!({"tool_call_id":"rejected", "name":"write_file", "ok":false}));
            let capture = SessionCapture {
                events: vec![JournalEvent {
                    event_type: "turn".into(),
                    raw: json!({
                        "event_type":"turn", "tool_calls":calls
                    }),
                }],
                ..Default::default()
            };
            let checks = crate::criteria::evaluate_deterministic_with_session(
                std::slice::from_ref(&criterion),
                &outcome,
                Some(&capture),
            );
            let projected = super::criteria(&checks, &outcome, Some(&capture)).unwrap();
            assert_eq!(projected[0]["passed"], count == 1);
            assert_eq!(
                projected[0]["diagnostic"],
                json!({"kind":"count","min":1,"max":1,"actual":count})
            );
            assert!(
                !serde_json::to_string(&projected)
                    .unwrap()
                    .contains("private-sentinel")
            );
        }
        for capture in [
            SessionCapture {
                skipped_lines: 1,
                ..Default::default()
            },
            SessionCapture {
                dropped_lines: 1,
                ..Default::default()
            },
            SessionCapture {
                integrity_errors: 1,
                ..Default::default()
            },
        ] {
            let checks = crate::criteria::evaluate_deterministic_with_session(
                std::slice::from_ref(&criterion),
                &outcome,
                Some(&capture),
            );
            let projected = super::criteria(&checks, &outcome, Some(&capture)).unwrap();
            assert_eq!(projected[0]["passed"], false);
            assert!(projected[0]["diagnostic"].is_null());
        }
        let checks =
            crate::criteria::evaluate_deterministic_with_session(&[criterion], &outcome, None);
        let projected = super::criteria(&checks, &outcome, None).unwrap();
        assert_eq!(projected[0]["passed"], false);
        assert!(projected[0]["diagnostic"].is_null());
    }

    #[test]
    fn tool_budget_progress_reports_the_same_count_as_the_verdict() {
        for (terminal, archive, expected, passed) in
            [(7, 30, 30, false), (30, 7, 30, false), (8, 7, 8, true)]
        {
            let outcome = RunOutcome {
                tool_calls_count: terminal,
                total_tool_calls: archive,
                ..RunOutcome::new("model")
            };
            let checks = fixture_evaluate(
                &[Criterion::ToolsCountBetween { min: 4, max: 18 }],
                &outcome,
            );
            let projected = fixture_criteria(&checks, &outcome).unwrap();
            assert_eq!(projected[0]["passed"], passed);
            assert_eq!(projected[0]["diagnostic"]["actual"], expected);
        }
    }

    #[test]
    fn missing_cache_scope_evidence_is_explained_without_publishing_private_data() {
        let mut outcome = RunOutcome::new("private-sentinel");
        outcome.stderr = "private-sentinel".into();
        let checks = fixture_evaluate(
            &[Criterion::PromptCacheReuseScope {
                scope: crate::case::PromptCacheReuseScope::ConversationTurns,
            }],
            &outcome,
        );
        let projected = fixture_criteria(&checks, &outcome).unwrap();
        assert_eq!(projected[0]["passed"], false);
        assert_eq!(projected[0]["diagnostic"]["kind"], "cache_scope");
        assert_eq!(projected[0]["diagnostic"]["observations"], Value::Null);
        assert_eq!(
            projected[0]["diagnostic"]["executions"][0]["capture"],
            "missing"
        );
        assert!(
            !serde_json::to_string(&projected)
                .unwrap()
                .contains("private-sentinel")
        );
        let mut bad = checks;
        let mut diagnostic: Value =
            serde_json::from_str(bad[0].full_detail.as_deref().unwrap()).unwrap();
        diagnostic["post_cold_cache_read_tokens"] = json!("private-sentinel");
        bad[0].full_detail = Some(diagnostic.to_string());
        assert!(fixture_criteria(&bad, &outcome).is_err());
    }

    #[test]
    fn journal_progress_exports_only_typed_evaluator_diagnostics() {
        let criterion = Criterion::JournalToolSuccessRatio {
            min: 0.9,
            min_calls: 3,
            allowed_failures: 1,
        };
        let diagnostic = json!({
            "kind":"journal_ratio", "min":0.9, "min_calls":3, "allowed_failures":1,
            "terminal":4, "succeeded":2, "failed":2, "pending":0, "adjusted_terminal":3
        });
        let mut checks = fixture_evaluate(&[criterion], &RunOutcome::new("model"));
        checks[0].full_detail = Some(diagnostic.to_string());
        checks[0].detail = "private-sentinel".into();
        let projected = fixture_criteria(&checks, &RunOutcome::new("model")).unwrap();
        assert_eq!(projected[0]["diagnostic"], diagnostic);
        assert_eq!(projected[0]["passed"], false);
        assert!(
            !serde_json::to_string(&projected)
                .unwrap()
                .contains("private-sentinel")
        );
        let mut unsafe_diagnostic = diagnostic;
        unsafe_diagnostic["raw_error"] = json!("private-sentinel");
        checks[0].full_detail = Some(unsafe_diagnostic.to_string());
        let projected = fixture_criteria(&checks, &RunOutcome::new("model")).unwrap();
        assert!(projected[0]["diagnostic"].is_null());
    }

    #[test]
    fn policy_mismatch_progress_preserves_verdict_without_publishing_values() {
        let policy = json!({"mode":"fixed", "selector":{
            "kind":"configured_name", "model_name":"configured"}});
        for (actual, reason, path) in [
            (json!({}), "missing", "/requested_model_policy"),
            (
                json!({"requested_model_policy":null}),
                "type_mismatch",
                "/requested_model_policy",
            ),
            (
                json!({"requested_model_policy":{"mode":"fixed", "selector":{
                "kind":"configured_name", "model_name":"private-sentinel"}}}),
                "scalar_mismatch",
                "/requested_model_policy/selector/model_name",
            ),
            (
                json!({"requested_model_policy":{"mode":"fixed", "selector":{
                "kind":"configured_name", "model_name":"configured", "private-sentinel":null}}}),
                "unexpected_members",
                "/requested_model_policy/selector",
            ),
        ] {
            let mut capture =
                serde_json::to_value(crate::execution_capture::tests::tool_capture()).unwrap();
            capture["transcript"]["items"][0]["tool_calls"][0]["arguments"] =
                json!(actual.to_string());
            let mut outcome = RunOutcome::new("model");
            outcome.run_id = Some("root".into());
            outcome.session_id = Some("session".into());
            let mut stream = crate::runner::StreamCapture::default();
            stream.identity_verified = true;
            stream.execution = Some(serde_json::from_value(capture).unwrap());
            outcome.stream_capture = Some(stream);
            let criterion: Criterion = serde_json::from_value(json!({
                "type":"execution_tool_count", "name":"agent", "min":1,"max":1,"root_only":true,
                "predicates":[{"document":"arguments","path":"/requested_model_policy","equals":policy}]
            })).unwrap();
            let results = fixture_evaluate(&[criterion], &outcome);
            let projected = fixture_criteria(&results, &outcome).unwrap();
            assert_eq!(projected[0]["passed"], false);
            let diagnostic = &projected[0]["diagnostic"];
            assert_eq!(diagnostic["actual"], 0);
            assert_eq!(diagnostic["predicate_matches"], json!([0]));
            assert_eq!(diagnostic["predicate_mismatches"][0]["failure_count"], 1);
            let detail = &diagnostic["predicate_mismatches"][0]["first_mismatch"];
            assert_eq!(detail["reason"], reason);
            assert_eq!(detail["path"], path);
            assert!(
                !serde_json::to_string(&projected)
                    .unwrap()
                    .contains("private-sentinel")
            );
        }
    }

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
        let results = fixture_evaluate(&[criterion], &outcome);
        let projected = fixture_criteria(&results, &outcome).unwrap();
        assert_eq!(projected[0]["passed"], false);
        assert_eq!(
            projected[0]["diagnostic"],
            json!({
                "kind":"tool_count", "min":1,"max":1,"actual":0,"considered":1,
                "expected_ok":false,"outcome_matches":1,"error_matches":null,"predicate_matches":[1,0,0],
                "predicate_mismatches":[
                    {"predicate_index":1,"failure_count":1,"first_mismatch":{
                        "path":"/error_kind","reason":"scalar_mismatch","expected_type":"string","actual_type":"string"}},
                    {"predicate_index":2,"failure_count":1,"first_mismatch":{
                        "path":"/status","reason":"scalar_mismatch","expected_type":"string","actual_type":"string"}}
                ],
                "observed_outcomes":[{"count":1,"outcome":{"disposition":"executed","ok":false,
                    "status":"rejected","error_kind":"tool_invalid_args","execution":"not_reported","validation":null}}],
                "outcomes_omitted":0
            })
        );
        assert!(
            !serde_json::to_string(&projected)
                .unwrap()
                .contains("private-sentinel")
        );
        for (content, status, error, execution) in [
            (
                r#"{"status":"timeout"}"#,
                "timeout",
                "not_reported",
                "not_reported",
            ),
            (
                r#"{"status":"still_running"}"#,
                "still_running",
                "not_reported",
                "not_reported",
            ),
            (
                r#"{"status":"launched"}"#,
                "launched",
                "not_reported",
                "not_reported",
            ),
            (
                r#"{"status":"private-sentinel","error_kind":"private-sentinel","executed":null}"#,
                "other",
                "other",
                "unknown",
            ),
            (
                r#"{"status":{},"error_kind":{},"executed":"private-sentinel"}"#,
                "not_reported",
                "other",
                "not_reported",
            ),
            (
                r#"{"status":"blocked","error_kind":"delegation_model_scope_unresolved","advisory":{"executed":false}}"#,
                "blocked",
                "delegation_model_scope_unresolved",
                "not_executed",
            ),
        ] {
            outcome
                .stream_capture
                .as_mut()
                .unwrap()
                .execution
                .as_mut()
                .unwrap()
                .transcript
                .as_mut()
                .unwrap()
                .items[1]
                .content = content.into();
            let value = fixture_criteria(&results, &outcome).unwrap();
            let observed = &value[0]["diagnostic"]["observed_outcomes"][0]["outcome"];
            assert_eq!(observed["status"], status);
            assert_eq!(observed["error_kind"], error);
            assert_eq!(observed["execution"], execution);
            assert!(
                !serde_json::to_string(&value)
                    .unwrap()
                    .contains("private-sentinel")
            );
        }
        outcome.stream_capture = None;
        assert_eq!(
            fixture_criteria(&results, &outcome).unwrap()[0]["diagnostic"]["kind"],
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
        let results = fixture_evaluate(&[composite], &outcome);
        let nested = fixture_criteria(&results, &outcome).unwrap();
        assert_eq!(
            nested[0]["diagnostic"]["checks"][1]["diagnostic"],
            json!({"kind":"count", "min":0,"max":45_000,"actual":80_000})
        );
    }

    #[test]
    fn schema_rejection_flows_from_real_validator_to_safe_canonical_progress() {
        let arguments = json!({"action":"spawn","prompt":"private-sentinel"});
        let error = astra_tools::schemas::validate_tool_arguments("agent", &arguments).unwrap_err();
        let receipt = error.rejection_output();
        let mut capture =
            serde_json::to_value(crate::execution_capture::tests::tool_capture()).unwrap();
        capture["transcript"]["items"][0]["tool_calls"][0]["arguments"] =
            arguments.to_string().into();
        capture["transcript"]["items"][1]["tool_result"]["status"] = "rejected".into();
        capture["transcript"]["items"][1]["content"] = receipt.to_string().into();
        let mut outcome = RunOutcome::new("model");
        outcome.run_id = Some("root".into());
        let mut stream = crate::runner::StreamCapture::default();
        stream.identity_verified = true;
        stream.execution = Some(serde_json::from_value(capture).unwrap());
        outcome.stream_capture = Some(stream);
        let criterion: Criterion = serde_json::from_value(json!({"type":"execution_tool_count",
            "name":"agent","min":1,"max":1,"ok":true,"root_only":true}))
        .unwrap();
        let results = fixture_evaluate(&[criterion], &outcome);
        let projected = fixture_criteria(&results, &outcome).unwrap();
        assert_eq!(projected[0]["passed"], false);
        let observed = &projected[0]["diagnostic"]["observed_outcomes"][0]["outcome"];
        assert_eq!(observed["validation"], receipt["validation"]);
        assert_eq!(observed["execution"], "not_executed");
        assert!(
            !serde_json::to_string(&projected)
                .unwrap()
                .contains("private-sentinel")
        );
    }

    #[test]
    fn tool_outcome_buckets_are_bounded_without_changing_exact_counts() {
        for with_validation in [false, true] {
            let mut capture = crate::execution_capture::tests::tool_capture();
            let kinds = [
                "rate_limit",
                "payment_required",
                "server_error",
                "auth",
                "context_window",
                "invalid_request",
                "contract_violation",
                "stream_idle",
                "provider_deadline",
                "stream_transport",
                "connection_pool_exhausted",
                "budget_exhausted",
                "tool_rounds_exhausted",
                "network",
                "tool_not_found",
                "tool_invalid_args",
                "policy_denied",
            ];
            capture.run_tree.runs[0].total_tool_calls = 20;
            let page = capture.transcript.as_mut().unwrap();
            let (request, result) = (page.items[0].clone(), page.items[1].clone());
            page.items.drain(..2);
            for index in 0..20 {
                let mut request = request.clone();
                let mut result = result.clone();
                let id = format!("call-{index}");
                request.item_seq = 10 + index * 2;
                result.item_seq = request.item_seq + 1;
                request.tool_calls[0].tool_use_id = id.clone();
                result.tool_result.as_mut().unwrap().tool_use_id = id;
                result.tool_result.as_mut().unwrap().status = Some("failed".into());
                result.content =
                    json!({"error_kind":kinds[if index < 17 {index as usize} else {0}],
                "status":"failed", "error":"private-sentinel"})
                    .to_string();
                if with_validation {
                    let codes = [
                        "required",
                        "type",
                        "const",
                        "enum",
                        "min_length",
                        "max_length",
                        "minimum",
                        "maximum",
                        "unique_items",
                        "min_items",
                        "max_items",
                        "union",
                        "additional_properties",
                        "action_fields",
                        "required_alternative",
                        "malformed_json",
                    ];
                    let issue = json!({"code":codes[index as usize % codes.len()],"path":"/requested_model_policy/selector/model_name"});
                    result.content = json!({"error_kind":"tool_invalid_args","status":"rejected",
                    "validation":{"issues":vec![issue;8],"omitted":0}})
                    .to_string();
                }
                page.items.extend([request, result]);
            }
            let mut outcome = RunOutcome::new("model");
            outcome.run_id = Some("root".into());
            let mut stream = crate::runner::StreamCapture::default();
            stream.identity_verified = true;
            stream.execution = Some(capture);
            outcome.stream_capture = Some(stream);
            let criterion: Criterion =
                serde_json::from_value(json!({"type":"execution_tool_count",
            "name":"agent", "min":1,"max":1,"ok":true,"root_only":true}))
                .unwrap();
            let results = fixture_evaluate(&[criterion], &outcome);
            let projected = fixture_criteria(&results, &outcome).unwrap();
            let diagnostic = &projected[0]["diagnostic"];
            assert_eq!(diagnostic["considered"], 20);
            assert_eq!(diagnostic["actual"], 0);
            assert_eq!(diagnostic["outcome_matches"], 0);
            let buckets = diagnostic["observed_outcomes"].as_array().unwrap();
            if with_validation {
                assert!(diagnostic["outcomes_omitted"].as_u64().unwrap() > 0);
                assert!(buckets.len() < 16);
            } else {
                assert_eq!(diagnostic["outcomes_omitted"], 1);
                assert_eq!(buckets.len(), 16);
            }
            assert!(serde_json::to_vec(buckets).unwrap().len() <= 8192);
            assert_eq!(
                buckets
                    .iter()
                    .map(|b| b["count"].as_u64().unwrap())
                    .sum::<u64>()
                    + diagnostic["outcomes_omitted"].as_u64().unwrap(),
                20
            );
            assert!(
                !serde_json::to_string(&projected)
                    .unwrap()
                    .contains("private-sentinel")
            );
        }
    }

    #[test]
    fn child_proof_reasons_preserve_verdicts_and_exclude_private_evidence() {
        for (fault, status, reason) in [
            ("none", "proven", None),
            ("count", "contradicted", Some("child_count_mismatch")),
            ("running", "contradicted", Some("child_not_completed")),
            ("model", "contradicted", Some("model_mismatch")),
            ("result", "contradicted", Some("expected_result_mismatch")),
            ("digest", "contradicted", Some("result_digest_mismatch")),
            ("late", "contradicted", Some("finalization_before_adoption")),
            ("tree", "unavailable", Some("run_tree_unavailable")),
            (
                "missing-result",
                "unavailable",
                Some("child_result_unavailable"),
            ),
            (
                "missing-adoption",
                "unavailable",
                Some("adoption_evidence_unavailable"),
            ),
            (
                "missing-finalization",
                "unavailable",
                Some("finalization_unavailable"),
            ),
            (
                "window-identity",
                "unavailable",
                Some("result_window_identity_conflict"),
            ),
            (
                "window-digest",
                "unavailable",
                Some("result_window_digest_conflict"),
            ),
            (
                "window-invalid",
                "unavailable",
                Some("result_window_invalid"),
            ),
            (
                "window-shape",
                "unavailable",
                Some("result_window_unavailable"),
            ),
            ("fanout", "proven", None),
            ("wrong-group", "contradicted", Some("fanout_group_mismatch")),
            ("wrong-slot", "contradicted", Some("slot_missing")),
            (
                "failed-start",
                "contradicted",
                Some("fanout_start_unsuccessful"),
            ),
            (
                "repeated-start",
                "contradicted",
                Some("fanout_start_repeated"),
            ),
        ] {
            let mut capture =
                serde_json::to_value(crate::execution_capture::tests::tool_capture()).unwrap();
            let mut criterion = json!({"type":"execution_child_results_adopted",
                "children":[{"model":"test-model", "expected_result":{"text":"observed-value"}}]});
            let fanout = matches!(
                fault,
                "fanout" | "wrong-group" | "wrong-slot" | "failed-start" | "repeated-start"
            );
            if fanout {
                criterion["fanout_group"] = json!({"kind":"id", "value":"group"});
                criterion["children"][0]["slot_index"] = 0.into();
                capture["transcript"]["items"][0]["tool_calls"][0]["name"] = "agent_fanout".into();
                capture["transcript"]["items"][0]["tool_calls"][0]["arguments"] =
                    json!({"action":"start", "group_id":"group", "description":"private-sentinel"})
                        .to_string()
                        .into();
                capture["transcript"]["items"][1]["tool_result"]["name"] = "agent_fanout".into();
                capture["transcript"]["items"][1]["content"] =
                    json!({"status":"started", "group_id":"group", "title":"group",
                        "fanout":{"group_id":"group", "title":"group", "parent_run_id":"root"},
                        "work_unit_observation":{"id":"group", "kind":"agent_fanout"}, "agents":[{
                        "slot_index":0, "run_id":"child", "agent_id":"member"
                    }]})
                    .to_string()
                    .into();
            }
            let facts = "/reflection/graph_slice/nodes/0/metadata/execution_spine/facts";
            match fault {
                "count" => criterion["children"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"model":"test-model"})),
                "running" => capture["run_tree"]["runs"][1]["status"] = "running".into(),
                "model" => criterion["children"][0]["model"] = "private-sentinel".into(),
                "result" => {
                    criterion["children"][0]["expected_result"]["text"] = "private-sentinel".into()
                }
                "digest" => {
                    capture.pointer_mut(facts).unwrap()[1]["children"][0]["result_sha256"] =
                        "private-sentinel".into()
                }
                "late" => {
                    capture.pointer_mut(facts).unwrap()[2]["created_at"] =
                        "2026-10-07T00:00:01.000000".into()
                }
                "tree" => capture["run_tree"]["truncated"] = true.into(),
                "missing-result" => {
                    capture["transcript"]["items"].as_array_mut().unwrap().pop();
                }
                "missing-adoption" => {
                    capture
                        .pointer_mut(facts)
                        .unwrap()
                        .as_array_mut()
                        .unwrap()
                        .remove(1);
                }
                "missing-finalization" => {
                    capture
                        .pointer_mut(facts)
                        .unwrap()
                        .as_array_mut()
                        .unwrap()
                        .pop();
                }
                "wrong-group" => {
                    let mut args: Value = serde_json::from_str(
                        capture["transcript"]["items"][0]["tool_calls"][0]["arguments"]
                            .as_str()
                            .unwrap(),
                    )
                    .unwrap();
                    args["group_id"] = "private-sentinel".into();
                    capture["transcript"]["items"][0]["tool_calls"][0]["arguments"] =
                        args.to_string().into();
                }
                "window-identity" | "window-digest" | "window-invalid" | "window-shape" => {
                    let entry = &mut capture.pointer_mut(facts).unwrap()[1]["children"][0];
                    entry["result_truncated"] = true.into();
                    entry["result_range"] = match fault {
                        "window-identity" => json!([0, 14, 999]),
                        "window-invalid" => json!([0, 999, 14]),
                        "window-shape" => json!("private-sentinel"),
                        _ => json!([0, 14, 14]),
                    };
                    if fault == "window-digest" {
                        entry["result_sha256"] = "private-sentinel".into();
                    }
                }
                "wrong-slot" => criterion["children"][0]["slot_index"] = 1.into(),
                "failed-start" => {
                    capture["transcript"]["items"][1]["tool_result"]["status"] = "failed".into()
                }
                "repeated-start" => {
                    let items = capture["transcript"]["items"].as_array_mut().unwrap();
                    let mut request = items[0].clone();
                    let mut response = items[1].clone();
                    request["item_seq"] = 4.into();
                    request["source_event_id"] = "source-4".into();
                    request["tool_calls"][0]["tool_use_id"] = "repeat".into();
                    response["item_seq"] = 5.into();
                    response["source_event_id"] = "source-5".into();
                    response["tool_result"]["tool_use_id"] = "repeat".into();
                    items.extend([request, response]);
                    capture["run_tree"]["runs"][0]["total_tool_calls"] = 2.into();
                }
                _ => {}
            }
            let mut outcome = RunOutcome::new("model");
            outcome.run_id = Some("root".into());
            outcome.text = "private-sentinel".into();
            outcome.stderr = "private-sentinel".into();
            let mut stream = crate::runner::StreamCapture::default();
            stream.identity_verified = true;
            stream.execution = Some(serde_json::from_value(capture).unwrap());
            outcome.stream_capture = Some(stream);
            let criterion: Criterion = serde_json::from_value(criterion).unwrap();
            let results = fixture_evaluate(&[criterion], &outcome);
            assert_eq!(results[0].passed, status == "proven", "{fault}");
            assert_eq!(
                crate::criteria::execution_verification_unavailable(&results[0]),
                status == "unavailable",
                "{fault}"
            );
            let projected = fixture_criteria(&results, &outcome).unwrap();
            assert_eq!(
                projected[0]["diagnostic"],
                json!({
                    "kind":"child_adoption", "status":status, "reason":reason
                }),
                "{fault}"
            );
            assert!(
                !serde_json::to_string(&projected)
                    .unwrap()
                    .contains("private-sentinel"),
                "{fault}"
            );
        }
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
            ) -> Result<crate::judger::JudgerResult, astra_core::ClassifiedError> {
                if self.0 {
                    return Err(astra_core::ClassifiedError::new(
                        astra_core::ErrorKind::ProviderDeadline,
                        "private-sentinel",
                    ));
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
            let projected = fixture_criteria(&[result], &outcome).unwrap();
            assert_eq!(projected[0]["passed"], false);
            assert_eq!(projected[0]["diagnostic"]["assessment"], expected);
            assert!(
                !serde_json::to_string(&projected)
                    .unwrap()
                    .contains("private-sentinel")
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_cli_judge_projects_only_typed_failure_through_required_quorum() {
        use crate::judger::{AstraCliJudger, JudgerConfig, QuorumAgg, QuorumJudger};
        let directory = tempfile::tempdir().unwrap();
        let shim = directory.path().join("judge");
        let criterion = Criterion::HardJudger {
            question: "private-sentinel".into(),
            threshold: 0.8,
            model: None,
        };
        let outcome = RunOutcome::new("model");
        for (wire_kind, expected_kind) in [
            ("provider_deadline", "provider_deadline"),
            ("rate_limit", "rate_limit"),
            ("private-sentinel", "unknown"),
        ] {
            crate::test_support::write_executable_shim(&shim, format!(
                "#!/bin/sh\nprintf '%s\\n' '{{\"ok\":false,\"stage\":\"completion\",\"error_kind\":\"{wire_kind}\",\"error\":\"private-sentinel\",\"text\":\"SCORE: 1.0\"}}'\nexit 5\n"
            )).unwrap();
            let judge = QuorumJudger::new(
                AstraCliJudger::new(JudgerConfig::new(&shim, "model")),
                2,
                QuorumAgg::Max,
            );
            let result = crate::judger::evaluate_judger(&judge, &criterion, &outcome, None)
                .await
                .unwrap();
            assert!(!result.passed);
            assert!(result.score.is_none());
            assert!(
                result
                    .full_detail
                    .as_ref()
                    .unwrap()
                    .contains("private-sentinel")
            );
            let projected = fixture_criteria(&[result], &outcome).unwrap();
            assert_eq!(
                projected[0]["diagnostic"],
                json!({
                    "kind":"judge", "threshold":0.8, "assessment":"unavailable",
                    "failure":{"error_kind":expected_kind,"stage":"completion"}
                })
            );
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
