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

pub const SCHEMA: &str = "astra.harness_progress.v5";
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
                        .unwrap_or_else(|failure| failure.diagnostic()),
                Criterion::DurationBetween { min_ms, max_ms } =>
                    json!({"kind":"count", "min":min_ms, "max":max_ms, "actual":outcome.duration_ms}),
                Criterion::ToolsCountBetween { min, max } =>
                    json!({"kind":"count", "min":min, "max":max, "actual":outcome.tool_calls_count}),
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
    fn missing_cache_scope_evidence_is_explained_without_publishing_private_data() {
        let mut outcome = RunOutcome::new("private-sentinel");
        outcome.stderr = "private-sentinel".into();
        let checks = crate::criteria::evaluate_deterministic(
            &[Criterion::PromptCacheReuseScope {
                scope: crate::case::PromptCacheReuseScope::ConversationTurns,
            }],
            &outcome,
        );
        let projected = criteria(&checks, &outcome).unwrap();
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
        assert!(criteria(&bad, &outcome).is_err());
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
            let results = crate::criteria::evaluate_deterministic(&[criterion], &outcome);
            let projected = criteria(&results, &outcome).unwrap();
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
        let results = crate::criteria::evaluate_deterministic(&[criterion], &outcome);
        let projected = criteria(&results, &outcome).unwrap();
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
            let value = criteria(&results, &outcome).unwrap();
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
        let results = crate::criteria::evaluate_deterministic(&[criterion], &outcome);
        let projected = criteria(&results, &outcome).unwrap();
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
            let results = crate::criteria::evaluate_deterministic(&[criterion], &outcome);
            let projected = criteria(&results, &outcome).unwrap();
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
            let results = crate::criteria::evaluate_deterministic(&[criterion], &outcome);
            assert_eq!(results[0].passed, status == "proven", "{fault}");
            assert_eq!(
                crate::criteria::execution_verification_unavailable(&results[0], &outcome),
                status == "unavailable",
                "{fault}"
            );
            let projected = criteria(&results, &outcome).unwrap();
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
            let projected = criteria(&[result], &outcome).unwrap();
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
