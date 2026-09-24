//! Reproducible comparisons through the canonical authenticated completion boundary.
use super::cli_args::ModelCompareArgs;
use astra_thin_client::{CompletionOperation, CompletionRequest, ThinClient, ThinClientError};
#[cfg(test)]
use astra_turn_types::{JUDGMENT_SCHEMA_VERSION, JudgmentResponse};
use astra_turn_types::{
    JudgmentNoulDecision, JudgmentQuestion, JudgmentRequest, JudgmentResponseProvenance,
    judgment_messages, normalize_judgment_response,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

const DEADLINE_MS: u64 = 3_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    operation: CompletionOperation,
    request: JudgmentRequest,
    #[serde(default)]
    expected: Option<Vec<String>>,
    #[serde(default = "default_threshold")]
    threshold: f64,
}
fn default_threshold() -> f64 {
    0.5
}
fn hash(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
fn binary_hash() -> Result<String, String> {
    let mut file = std::fs::File::open(std::env::current_exe().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let mut digest = Sha256::new();
    let mut buf = [0u8; 65_536];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}
fn validate_cases(cases: &[Case]) -> Result<(), String> {
    if cases.is_empty() {
        return Err("The case file is empty. Provide at least one judgment case.".into());
    }
    let mut seen = HashSet::new();
    for case in cases {
        if case.id.trim().is_empty() || !seen.insert(&case.id) {
            return Err("Case IDs must be nonempty and unique.".into());
        }
        if case.operation == CompletionOperation::MemoryExtraction {
            return Err(format!(
                "Case {}: memory extraction is not a judgment operation.",
                case.id
            ));
        }
        case.request
            .validate()
            .map_err(|e| format!("Case {}: {e}", case.id))?;
        if case
            .request
            .questions
            .values()
            .any(|question| !matches!(question, JudgmentQuestion::Noul { .. }))
        {
            return Err(format!(
                "Case {}: this comparison supports Noul questions only.",
                case.id
            ));
        }
        if !case.threshold.is_finite() || !(0.0..=1.0).contains(&case.threshold) {
            return Err(format!(
                "Case {}: threshold must be between 0 and 1.",
                case.id
            ));
        }
        if let Some(expected) = &case.expected {
            let mut ids = HashSet::new();
            if expected
                .iter()
                .any(|id| !case.request.questions.contains_key(id) || !ids.insert(id))
            {
                return Err(format!(
                    "Case {}: expected answers must be unique question IDs.",
                    case.id
                ));
            }
        }
    }
    Ok(())
}
fn selection(
    text: &str,
    case: &Case,
    model: &str,
    provenance: Option<JudgmentResponseProvenance>,
) -> Result<Vec<String>, String> {
    let normalized = normalize_judgment_response(&case.request, text, model, provenance)
        .map_err(|_| "invalid_judgment".to_owned())?;
    let mut selected = Vec::new();
    for (id, answer) in normalized.response.answers {
        let selected_answer = match normalized.provenance {
            JudgmentResponseProvenance::ProviderProbability => {
                answer
                    .native_noul_probability()
                    .ok_or_else(|| "invalid_judgment".to_owned())?
                    > case.threshold
            }
            JudgmentResponseProvenance::DiscreteDecision => {
                match answer
                    .discrete_noul_decision()
                    .ok_or_else(|| "invalid_judgment".to_owned())?
                {
                    JudgmentNoulDecision::Yes => true,
                    JudgmentNoulDecision::No => false,
                    JudgmentNoulDecision::Unknown => {
                        return Err("uncertain_judgment".to_owned());
                    }
                }
            }
        };
        if selected_answer {
            selected.push(id);
        }
    }
    selected.sort();
    Ok(selected)
}
fn built_in_cases() -> Vec<Case> {
    use astra_runtime::memory_hooks::relevance::{
        build_memory_feedback_query, build_relevance_query,
    };
    let items = vec![
        "Always run cargo test before finishing code changes".into(),
        "The user prefers concise answers".into(),
    ];
    let feedback = [
        ("continue-en", "Continue", vec![]),
        ("continue-zh", "继续", vec![]),
        ("task-change", "Now explain PostgreSQL indexes", vec![]),
        (
            "current-exception",
            "Do not run tests for this documentation-only task",
            vec![],
        ),
        (
            "explicit-en",
            "The lesson about always running cargo test is wrong; stop using it",
            vec!["0"],
        ),
        (
            "explicit-zh",
            "之前那条始终运行cargo test的经验不适用了，以后不要再用。",
            vec!["0"],
        ),
        (
            "second-lesson",
            "I no longer prefer concise answers; remove that preference",
            vec!["1"],
        ),
        ("ambiguous", "不是这个问题", vec![]),
    ];
    let relevance = [
        (
            "relevance-code",
            "Run cargo tests for the Rust code changes",
            vec!["0", "1"],
        ),
        (
            "relevance-zh",
            "运行Rust代码修改的cargo测试，简短告诉我结果",
            vec!["0", "1"],
        ),
        ("relevance-greeting", "hi", vec![]),
        (
            "relevance-explanation",
            "Briefly explain PostgreSQL indexes",
            vec!["1"],
        ),
    ];
    feedback
        .into_iter()
        .map(|(id, message, expected)| (id, build_memory_feedback_query(message, &items), expected))
        .chain(
            relevance.into_iter().map(|(id, message, expected)| {
                (id, build_relevance_query(message, &items), expected)
            }),
        )
        .map(|(id, query, expected)| Case {
            id: id.into(),
            operation: CompletionOperation::MemoryRetrievalRerank,
            request: serde_json::from_str(&query).expect("shared typed builder"),
            expected: Some(expected.into_iter().map(str::to_owned).collect()),
            threshold: default_threshold(),
        })
        .collect()
}
fn order(repetition: u32) -> [usize; 2] {
    if repetition.is_multiple_of(2) {
        [0, 1]
    } else {
        [1, 0]
    }
}
fn error_code(error: ThinClientError) -> String {
    match error {
        ThinClientError::Api { status, .. } => format!("HTTP {}", status.as_u16()),
        _ => "transport_unavailable".into(),
    }
}
fn save(path: &Path, value: &Value) -> Result<(), String> {
    std::fs::write(
        path,
        serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("Could not save {}: {e}", path.display()))
}

pub(super) async fn run(
    api: &ThinClient,
    token: &str,
    args: &ModelCompareArgs,
) -> Result<(), String> {
    for id in [&args.baseline, &args.candidate] {
        astra_services::validate_model_offering_id(id).map_err(|_| {
            "Select an exact Offering ID from `astra admin model list`.".to_string()
        })?;
    }
    if args.baseline == args.candidate {
        return Err("Choose two different Offerings to compare.".into());
    }
    let cases = match &args.cases {
        Some(path) => serde_json::from_slice::<Vec<Case>>(
            &std::fs::read(path).map_err(|e| format!("Could not read {}: {e}", path.display()))?,
        )
        .map_err(|e| format!("Invalid case file: {e}"))?,
        None => built_in_cases(),
    };
    validate_cases(&cases)?;
    let calls = (cases.len() as u64)
        .checked_mul(u64::from(args.repeat))
        .and_then(|n| n.checked_mul(2))
        .filter(|n| *n <= u64::from(u32::MAX))
        .ok_or("Comparison exceeds the inference coordinate range.")?;
    let catalog: Value = serde_json::from_str(
        &super::session_runtime::load_server_model_catalog_json(
            api,
            token,
            astra_core::model_wire::purpose::ModelCatalogPurpose::TypedJudgment,
        )
        .await?,
    )
    .map_err(|e| e.to_string())?;
    let items = catalog["items"]
        .as_array()
        .ok_or("Model catalog is malformed.")?;
    let mut offerings = Vec::new();
    for id in [&args.baseline, &args.candidate] {
        let item = items.iter().find(|v| v["offering_id"].as_str() == Some(id.as_str())).ok_or_else(|| format!("Offering {id} is unavailable to this login. Run `astra admin model list` to choose an available Offering."))?;
        offerings.push(json!({"offering_id":id,"name":item["name"],"provider":item["provider"]}));
    }
    let output = args.output.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("astra-judgment-compare-{}", uuid::Uuid::new_v4()))
    });
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&output).map_err(|e| {
        format!(
            "Choose a new output directory; could not create {}: {e}",
            output.display()
        )
    })?;
    let manifest = json!({"schema_version":1,"astra_version":env!("CARGO_PKG_VERSION"),"binary_hash":binary_hash()?,"fixture_hash":hash(&serde_json::to_vec(&cases).map_err(|e| e.to_string())?),"cases":cases,"offerings":offerings,"repeat":args.repeat,"deadline_ms":DEADLINE_MS,"output_format":"typed-judgment-v1; chat true/uncertain IDs; native probabilities","backend_order":"alternates_per_repetition","cost":"Usage only; completion catalog does not expose pricing. No invoice estimate is inferred."});
    save(&output.join("manifest.json"), &manifest)?;
    let session = api
        .create_session(
            Some(token),
            &astra_thin_client::SessionCreateRequest {
                title: Some("Model judgment comparison".into()),
                ..Default::default()
            },
        )
        .await
        .map_err(super::map_thin_err)?;
    let session_id = session["session_id"]
        .as_str()
        .ok_or("Comparison session response omitted session_id.")?;
    let mut results =
        std::fs::File::create(output.join("results.jsonl")).map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    stdout_println!(
        "Compare judgments · {} vs {} · {} cases × {} repeats",
        offerings[0]["name"].as_str().unwrap_or("baseline"),
        offerings[1]["name"].as_str().unwrap_or("candidate"),
        cases.len(),
        args.repeat
    );
    stdout_println!("Reports: {}", output.display());
    let mut sequence = 0u32;
    for repetition in 0..args.repeat {
        for case in &cases {
            let messages = judgment_messages(&case.request);
            let input_hash = hash(&serde_json::to_vec(&messages).map_err(|e| e.to_string())?);
            for backend in order(repetition) {
                let id = if backend == 0 {
                    &args.baseline
                } else {
                    &args.candidate
                };
                let mut request = CompletionRequest::new(
                    case.operation,
                    session_id,
                    0,
                    0,
                    sequence,
                    messages.clone(),
                )
                .with_offering_id(id)
                .with_timeout(Duration::from_millis(DEADLINE_MS));
                request.max_tokens =
                    u32::try_from(case.request.output_token_budget()).map_err(|_| {
                        "Judgment output budget exceeds the completion protocol".to_string()
                    })?;
                request.temperature = 0.0;
                let started = Instant::now();
                let mut row = json!({"repeat":repetition+1,"case_id":case.id,"backend":if backend==0 {"baseline"} else {"candidate"},"offering_id":id,"input_hash":input_hash,"operation":case.operation,"session_id":session_id,"logical_attempt":sequence,"threshold":case.threshold});
                match api.post_completions(token, &request).await {
                    Ok(response) => {
                        let text = response.first_text().unwrap_or("");
                        let finish_reason = response
                            .choices
                            .first()
                            .map(|choice| choice.finish_reason.clone());
                        row["raw_output"] = json!(text);
                        row["model"] = json!(response.model);
                        row["usage"] = json!(response.usage);
                        row["finish_reason"] = json!(finish_reason);
                        if finish_reason.as_deref() != Some("stop") {
                            row["status"] = json!("incomplete");
                            row["error"] = json!("completion_did_not_finish_normally");
                        } else {
                            match selection(
                                text,
                                case,
                                &response.model,
                                response.judgment_provenance,
                            ) {
                                Ok(selected) => {
                                    row["status"] = json!("valid");
                                    if let Some(expected) = &case.expected {
                                        let mut expected = expected.clone();
                                        expected.sort();
                                        row["matches_expected"] = json!(selected == expected);
                                    }
                                    row["selected"] = json!(selected);
                                }
                                Err(reason) => {
                                    row["status"] = json!("invalid_answer");
                                    row["error"] = json!(reason);
                                }
                            }
                        }
                    }
                    Err(error) => {
                        row["status"] = json!("unavailable");
                        row["error"] = json!(error_code(error));
                    }
                }
                row["elapsed_ms"] = json!(started.elapsed().as_millis() as u64);
                writeln!(results, "{}", row)
                    .and_then(|_| results.flush())
                    .map_err(|e| format!("Could not save comparison result: {e}"))?;
                rows.push(row);
                sequence += 1;
                stdout_println!(
                    "  {sequence}/{calls} · {} · {} · {}",
                    case.id,
                    if backend == 0 {
                        "baseline"
                    } else {
                        "candidate"
                    },
                    rows.last().expect("just appended")["status"]
                        .as_str()
                        .unwrap_or("unknown")
                );
            }
        }
    }
    let mut summary = json!({"session_id":session_id,"report_directory":output});
    for backend in ["baseline", "candidate"] {
        let subset = rows
            .iter()
            .filter(|r| r["backend"] == backend)
            .collect::<Vec<_>>();
        let mut latencies = subset
            .iter()
            .map(|r| {
                r["elapsed_ms"]
                    .as_u64()
                    .expect("every saved call has elapsed time")
            })
            .collect::<Vec<_>>();
        latencies.sort();
        let valid = subset.iter().filter(|r| r["status"] == "valid").count();
        let timed = latencies.len();
        let median = if timed == 0 {
            None
        } else if timed.is_multiple_of(2) {
            Some((latencies[timed / 2 - 1] as f64 + latencies[timed / 2] as f64) / 2.0)
        } else {
            Some(latencies[timed / 2] as f64)
        };
        let reported = subset
            .iter()
            .filter_map(|r| {
                Some((
                    r["usage"]["prompt_tokens"].as_u64()?,
                    r["usage"]["completion_tokens"].as_u64()?,
                ))
            })
            .collect::<Vec<_>>();
        summary[backend] = json!({"calls":subset.len(),"valid":valid,"unavailable":subset.iter().filter(|r|r["status"]=="unavailable").count(),"incomplete":subset.iter().filter(|r|r["status"]=="incomplete").count(),"invalid_answer":subset.iter().filter(|r|r["status"]=="invalid_answer").count(),"assessed":subset.iter().filter(|r|r.get("matches_expected").is_some()).count(),"matches_expected":subset.iter().filter(|r|r["matches_expected"]==true).count(),"latency_population":"all_calls","median_ms":median,"p95_ms":if timed==0 {None} else {Some(latencies[(timed*95).div_ceil(100)-1])},"usage_reported_calls": reported.len(), "usage_status": if reported.len()==subset.len() {"complete"} else if reported.is_empty() {"unavailable"} else {"partial"}, "input_tokens": if reported.is_empty() {None} else {Some(reported.iter().map(|(input, _)| input).sum::<u64>())}, "output_tokens": if reported.is_empty() {None} else {Some(reported.iter().map(|(_, output)| output).sum::<u64>())}});
        stdout_println!(
            "{backend}: {valid}/{} valid · {} matching · median {} ms · P95 {} ms",
            subset.len(),
            summary[backend]["matches_expected"],
            summary[backend]["median_ms"],
            summary[backend]["p95_ms"]
        );
    }
    save(&output.join("summary.json"), &summary)?;
    stdout_println!(
        "Comparison complete. Summary: {}",
        output.join("summary.json").display()
    );
    if rows.iter().any(|r| r["status"] != "valid") {
        return Err("Some requests were unavailable or invalid; all observations were saved. Check summary.json and results.jsonl before drawing conclusions.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn comparison_saves_failures_and_never_calls_missing_usage_zero() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{body_partial_json, header, method, path},
        };
        let server = MockServer::start().await;
        let item = |id: &str, provider: &str| json!({"offering_id":id,"access_id":format!("access-{id}"),"access_kind":"self_hosted","access_label":"test","execution_placement":"server","name":id,"provider":provider,"description":null,"is_active":true,"context_window":64000,"max_completion_tokens":512,"architecture":null,"thinking_capability":null});
        Mock::given(method("GET")).and(path("/models")).and(header("authorization", "Bearer sentinel-token")).and(wiremock::matchers::query_param("purpose", "typed_judgment"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items":[item("offer-baseline","openai"),item("offer-jev","typesafe")],"total":2,"limit":200,"next_cursor":null,"catalog_revision":"test-revision"})))
            .expect(2).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/sessions"))
            .and(header("authorization", "Bearer sentinel-token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"session_id":"session-comparison"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let completion = |id: &str, text: &str| json!({"id":"response","object":"chat.completion","offering_id":id,"model":id,"judgment_provenance":"discrete_decision","choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}]});
        let truncated_completion = |id: &str, text: &str| json!({"id":"response","object":"chat.completion","offering_id":id,"model":id,"choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"length"}]});
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sentinel-token"))
            .and(body_partial_json(
                json!({"model_selection":{"offering_id":"offer-baseline"}}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(completion(
                "offer-baseline",
                r#"{"answers":{"0":{"type":"discrete_noul","decision":"yes"},"1":{"type":"discrete_noul","decision":"no"}}}"#,
            )))
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sentinel-token"))
            .and(body_partial_json(json!({"logical_attempt":1})))
            .respond_with(
                ResponseTemplate::new(429)
                    .set_body_json(json!({"error":"private upstream detail"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sentinel-token"))
            .and(body_partial_json(json!({"logical_attempt":2})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(truncated_completion(
                    "offer-jev",
                    r#"{"answers":{"0":{"type":"discrete_noul","decision":"yes"}}}"#,
                )),
            )
            .expect(1)
            .mount(&server)
            .await;
        let directory = tempfile::tempdir().unwrap();
        let cases = directory.path().join("cases.json");
        save(&cases, &json!([built_in_cases()[0]])).unwrap();
        let output = directory.path().join("report");
        let args = ModelCompareArgs {
            baseline: "offer-baseline".into(),
            candidate: "offer-jev".into(),
            cases: Some(cases),
            repeat: 2,
            output: Some(output.clone()),
        };
        let api = ThinClient::new(&server.uri(), None).unwrap();
        assert!(
            run(&api, "sentinel-token", &args)
                .await
                .unwrap_err()
                .contains("observations were saved")
        );
        let summary: Value =
            serde_json::from_slice(&std::fs::read(output.join("summary.json")).unwrap()).unwrap();
        assert_eq!(summary["candidate"]["unavailable"], 1);
        assert_eq!(summary["candidate"]["incomplete"], 1);
        assert_eq!(summary["candidate"]["invalid_answer"], 0);
        assert_eq!(summary["candidate"]["valid"], 0);
        assert_eq!(summary["candidate"]["latency_population"], "all_calls");
        assert!(summary["candidate"]["median_ms"].is_number());
        assert!(summary["candidate"]["p95_ms"].is_number());
        assert_eq!(summary["baseline"]["valid"], 2);
        assert_eq!(summary["baseline"]["usage_status"], "unavailable");
        assert!(summary["baseline"]["input_tokens"].is_null());
        let report = std::fs::read_to_string(output.join("results.jsonl")).unwrap();
        assert!(!report.contains("private upstream detail"));
        assert!(!report.contains("sentinel-token"));
        let rows = report
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 4);
        let failed_latencies = rows
            .iter()
            .filter(|row| row["backend"] == "candidate")
            .map(|row| row["elapsed_ms"].as_u64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            summary["candidate"]["median_ms"].as_f64().unwrap(),
            failed_latencies.iter().sum::<u64>() as f64 / failed_latencies.len() as f64
        );
        assert_eq!(
            summary["candidate"]["p95_ms"].as_u64().unwrap(),
            *failed_latencies.iter().max().unwrap()
        );
        assert!(
            rows.windows(2)
                .all(|pair| pair[0]["input_hash"] == pair[1]["input_hash"])
        );
        let requests = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| request.url.path() == "/v1/chat/completions")
            .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(requests.len(), 4);
        for (index, request) in requests.iter().enumerate() {
            assert_eq!(request["logical_attempt"], index);
            assert_eq!(request["session_id"], "session-comparison");
        }
        assert_eq!(
            requests
                .iter()
                .map(|request| request["model_selection"]["offering_id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["offer-baseline", "offer-jev", "offer-jev", "offer-baseline"]
        );
        assert!(
            run(&api, "sentinel-token", &args)
                .await
                .unwrap_err()
                .contains("new output directory")
        );
        assert_eq!(
            std::fs::read_to_string(output.join("results.jsonl")).unwrap(),
            report
        );
    }

    #[test]
    fn shared_cases_and_scoring_preserve_probabilities() {
        let cases = built_in_cases();
        validate_cases(&cases).unwrap();
        let mut case = cases[0].clone();
        let response = JudgmentResponse {
            schema_version: JUDGMENT_SCHEMA_VERSION,
            model: "model".into(),
            answers: [
                (
                    "0".into(),
                    astra_turn_types::JudgmentAnswer::Noul { noul: 0.6 },
                ),
                (
                    "1".into(),
                    astra_turn_types::JudgmentAnswer::Noul { noul: 0.1 },
                ),
            ]
            .into(),
        };
        let text = serde_json::to_string(&response).unwrap();
        assert_eq!(
            selection(
                &text,
                &case,
                "native-fixture",
                Some(JudgmentResponseProvenance::ProviderProbability)
            )
            .unwrap(),
            vec!["0"]
        );
        case.threshold = 0.8;
        assert!(
            selection(
                &text,
                &case,
                "native-fixture",
                Some(JudgmentResponseProvenance::ProviderProbability)
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            selection(
                r#"{"answers":{"unknown":{"type":"discrete_noul","decision":"yes"}}}"#,
                &case,
                "chat-fixture",
                Some(JudgmentResponseProvenance::DiscreteDecision)
            )
            .is_err()
        );
        assert_eq!(
            selection(
                r#"{"answers":{"0":{"type":"discrete_noul","decision":"unknown"},"1":{"type":"discrete_noul","decision":"no"}}}"#,
                &case,
                "chat-fixture",
                Some(JudgmentResponseProvenance::DiscreteDecision)
            )
            .unwrap_err(),
            "uncertain_judgment"
        );
        assert_eq!(order(0), [0, 1]);
        assert_eq!(order(1), [1, 0]);
    }
    #[test]
    fn fixtures_reject_unsupported_operations_and_unknown_expected_ids() {
        let mut cases = built_in_cases();
        cases[0].expected = Some(vec!["missing".into()]);
        assert!(validate_cases(&cases).is_err());
        cases[0].expected = None;
        cases[0].operation = CompletionOperation::MemoryExtraction;
        assert!(validate_cases(&cases).is_err());
    }

    #[test]
    fn fixtures_reject_choice_and_score_before_comparison() {
        let mut cases = built_in_cases();
        cases[0].request.questions.insert(
            "route".into(),
            JudgmentQuestion::Choice {
                instructions: "Choose a route".into(),
                criteria: [("a".into(), json!("A")), ("b".into(), json!("B"))].into(),
            },
        );
        assert!(
            validate_cases(&cases)
                .unwrap_err()
                .contains("Noul questions only")
        );

        cases[0].request.questions.remove("route");
        cases[0].request.questions.insert(
            "progress".into(),
            JudgmentQuestion::Score {
                instructions: "Rate progress".into(),
                criteria: vec![json!("low"), json!("high")],
            },
        );
        assert!(
            validate_cases(&cases)
                .unwrap_err()
                .contains("Noul questions only")
        );
    }
}
