//! TypeSafe System One protocol adapter. Business questions and policies belong to callers.
use super::client::LlmCallResult;
use astra_core::{ClassifiedError, ErrorKind};
use astra_turn_types::{
    JUDGMENT_SCHEMA_VERSION, JudgmentRequest, judgment_request_from_messages,
    normalize_judgment_response, parse_unique_judgment_json,
};
use serde::Deserialize;
use serde_json::{Value, json};

fn invalid(message: &'static str) -> ClassifiedError {
    ClassifiedError::new(ErrorKind::ContractViolation, message)
}

pub(super) struct NativeJudgmentRequest {
    pub body: Value,
    pub contract: JudgmentRequest,
}

pub(super) fn request(
    messages: &[Value],
    model: &str,
) -> Result<NativeJudgmentRequest, ClassifiedError> {
    let mut judgment: JudgmentRequest =
        judgment_request_from_messages(messages).map_err(|error| match error {
            astra_turn_types::JudgmentCodecError::Json(_) => {
                invalid("Invalid typed judgment schema")
            }
            astra_turn_types::JudgmentCodecError::Invalid(reason) => invalid(reason),
        })?;
    let mut questions =
        serde_json::to_value(&judgment.questions).expect("validated questions serialize");
    for question in questions
        .as_object_mut()
        .expect("question map")
        .values_mut()
    {
        question
            .as_object_mut()
            .expect("typed question")
            .remove("optional");
    }
    let state = std::mem::replace(&mut judgment.state, json!({}));
    Ok(NativeJudgmentRequest {
        body: json!({"model": model, "state": state, "questions": questions}),
        // The response needs the closed answer contract, not another full
        // copy of the user evidence across provider I/O.
        contract: judgment,
    })
}

#[derive(Deserialize)]
struct Response {
    model: String,
    answers: serde_json::Map<String, Value>,
    #[serde(default)]
    assessment: Option<Value>,
    #[serde(default)]
    usage: Option<Value>,
}

pub(super) fn response(
    raw: &[u8],
    request: &JudgmentRequest,
) -> Result<LlmCallResult, ClassifiedError> {
    let value = parse_unique_judgment_json(raw)
        .map_err(|_| invalid("Malformed TypeSafe judgment response"))?;
    let response: Response = serde_json::from_value(value)
        .map_err(|_| invalid("Malformed TypeSafe judgment response"))?;
    let native = json!({
        "schema_version": JUDGMENT_SCHEMA_VERSION,
        "model": response.model,
        "answers": response.answers,
    });
    let judgment = normalize_judgment_response(
        request,
        &native.to_string(),
        &response.model,
        Some(astra_turn_types::JudgmentResponseProvenance::ProviderProbability),
    )
    .map_err(|_| invalid("Invalid TypeSafe judgment response"))?
    .response;
    let mut judgment = serde_json::to_value(&judgment).expect("validated judgment serialization");
    // The caller owns optional observational validation. Preserve its payload
    // without allowing it to bypass the typed answer validation above.
    if let Some(assessment) = response.assessment {
        judgment["assessment"] = assessment;
    }
    // Billing metadata does not decide whether a valid judgment succeeded.
    // Preserve raw fields until the shared disjoint decoder qualifies them.
    let mut raw_usage = serde_json::Map::new();
    for key in [
        "input_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
        "output_tokens",
    ] {
        if let Some(value) = response.usage.as_ref().and_then(|u| u.get(key)) {
            raw_usage.insert(key.into(), value.clone());
        }
    }
    let (usage, usage_presence) = crate::turn::token_usage::parse_usage(
        crate::turn::token_usage::UsageDialect::AnthropicMessages,
        &raw_usage,
    )
    .map(|(usage, presence)| {
        let (usage, presence) = usage.qualified_snapshot(presence);
        (usage.to_qualified_json_map(presence), presence)
    })
    .unwrap_or_default();
    Ok(LlmCallResult {
        judgment_provenance: Some(
            astra_turn_types::JudgmentResponseProvenance::ProviderProbability,
        ),
        full_text: serde_json::to_string(&judgment).expect("validated judgment serialization"),
        model_used: response.model,
        usage_presence,
        usage,

        finish_reason: Some("stop".into()),
        ..LlmCallResult::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_hooks::relevance::{filter_memories, select_dismissed_memory_indices};
    use crate::memory_hooks::{DirectMemoryInferenceClient, MemoryInferencePort};
    use astra_turn_types::JudgmentResponse;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    fn scope() -> astra_turn_types::InferenceInvocationScope {
        astra_turn_types::InferenceInvocationScope::Session {
            session_id: "typesafe-pilot".into(),
            turn: 1,
            round: 0,
            operation_id: "memory_feedback".into(),
            logical_attempt: 0,
        }
    }
    fn client(base_url: String, api_key: String) -> DirectMemoryInferenceClient {
        DirectMemoryInferenceClient {
            fixed_temperature: None,
            thinking_protocol: None,
            base_url,
            api_key,
            model_name: "jev-1.13.0".into(),
            wire_model_name: None,
            provider: "typesafe".into(),
            header_overrides: HashMap::new(),
            request_body_overrides: None,
            completions_url_override: None,
            request_timeout: None,
        }
    }
    fn messages() -> Vec<Value> {
        vec![
            json!({"role":"user", "content": serde_json::to_string(&JudgmentRequest {
            schema_version: JUDGMENT_SCHEMA_VERSION, state: json!({"lesson":"example"}),
            questions: [("0".into(), astra_turn_types::JudgmentQuestion::Noul { instructions: "Is this useful?".into(), criteria: None })].into(),
        }).unwrap()}),
        ]
    }
    fn good() -> Value {
        json!({"model":"jev-1.13.0", "answers":{"0":{"type":"noul","noul":0.9}}, "usage":{"input_tokens":100,"output_tokens":4}})
    }

    fn decode_response(
        value: &Value,
        request: &JudgmentRequest,
    ) -> Result<LlmCallResult, ClassifiedError> {
        response(&serde_json::to_vec(value).unwrap(), request)
    }

    #[test]
    fn rejects_untyped_or_unknown_envelopes() {
        for content in [
            "plain prompt",
            r#"{"schema_version":2,"kind":"explicit_dismissal","user_message":"x","candidates":["a"]}"#,
            r#"{"schema_version":1,"kind":"unknown","user_message":"x","candidates":["a"]}"#,
        ] {
            let mut m = messages();
            m[0]["content"] = json!(content);
            assert!(request(&m, "jev-1.13.0").is_err());
        }
    }
    #[test]
    fn response_requires_valid_identity_probabilities_and_usage() {
        let req = request(&messages(), "jev-1.13.0").unwrap();
        for v in [
            json!({}),
            json!({"model":"m","answers":{"0":null},"usage":{"input_tokens":1,"output_tokens":1}}),
            json!({"model":"m","answers":{"0":{"type":"noul","noul":1.1}},"usage":{"input_tokens":1,"output_tokens":1}}),
            json!({"model":"m","answers":{"1":{"type":"noul","noul":0.9}},"usage":{"input_tokens":1,"output_tokens":1}}),
        ] {
            assert!(decode_response(&v, &req.contract).is_err());
        }
        let result = decode_response(&good(), &req.contract).unwrap();
        let decoded: JudgmentResponse = serde_json::from_str(&result.full_text).unwrap();
        assert_eq!(decoded.answers["0"].native_noul_probability(), Some(0.9));
        assert_eq!(result.usage["input_tokens"], 100);
        assert_eq!(result.usage["output_tokens"], 4);
        assert!(!result.usage.contains_key("cached_input_tokens"));
        assert!(!result.usage.contains_key("cache_creation_tokens"));
        assert!(!result.usage.contains_key("total_tokens"));
        let terminal = crate::turn::llm::client::provider_attempt_terminal_from_result(&result);
        assert_eq!(terminal.usage.input.fresh_input_tokens, 100);
        assert_eq!(terminal.usage.output_tokens, 4);
        assert_eq!(
            terminal.usage_status,
            astra_services::InferenceUsageStatus::ProviderPartial
        );
        assert_eq!(result.model_used, "jev-1.13.0");
    }

    #[test]
    fn work_admission_assessment_survives_adapter_and_remains_optional() {
        let judgment = astra_services::work_admission_classification_request(&Default::default());
        let req = request(
            &astra_services::work_admission_classification_messages(&judgment),
            "jev-1.13.0",
        )
        .unwrap();
        assert!(
            req.body["questions"]
                .as_object()
                .unwrap()
                .values()
                .all(|question| question.get("optional").is_none()),
            "local abstention policy must not change the native provider protocol"
        );
        let answers: serde_json::Map<String, Value> = judgment
            .questions
            .iter()
            .map(|(key, question)| {
                let yes = matches!(
                    key.as_str(),
                    "mutation.read_only" | "scope.unknown" | "domain.none"
                );
                let answer = match question {
                    astra_turn_types::JudgmentQuestion::Noul { .. } =>
                        json!({"type":"noul", "noul": if yes { 1.0 } else { 0.0 }}),
                    astra_turn_types::JudgmentQuestion::Choice { criteria, .. } => {
                        let neutral = if criteria.contains_key("unknown") { "unknown" } else { "none" };
                        let probabilities: serde_json::Map<String, Value> = criteria.keys()
                            .map(|option| (option.clone(), json!(if option == neutral { 1.0 } else { 0.0 })))
                            .collect();
                        json!({"type":"choice", "choice":neutral, "probabilities":probabilities, "confidence":1.0})
                    }
                    _ => unreachable!("Work judgment uses Noul and Choice"),
                };
                (key.clone(), answer)
            })
            .collect();
        for (assessment, expected) in [
            (
                json!({"difficulty":"easy"}),
                Some(astra_turn_types::TurnAssessment {
                    difficulty: astra_turn_types::TaskDifficulty::Easy,
                    ..Default::default()
                }),
            ),
            (Value::Null, None),
            (json!("invalid"), None),
            (json!({"difficulty":"invalid"}), None),
            (json!({"unexpected":true}), None),
        ] {
            let mut wire =
                json!({"model":"jev-1.13.0", "answers":answers, "assessment":assessment});
            let result = decode_response(&wire, &req.contract).unwrap();
            let classification = astra_services::parse_work_admission_classification(
                &judgment,
                &result.full_text,
                &result.model_used,
                result.judgment_provenance,
            )
            .unwrap();
            assert_eq!(
                classification.into_not_required().unwrap().assessment(),
                expected
            );

            for invalid in [
                None,
                Some(Value::Null),
                Some(json!({"type":"discrete_choice","option":"correct"})),
                Some(
                    json!({"type":"choice","choice":"correct","probabilities":{"correct":0.5},"confidence":1.0}),
                ),
                Some(json!({"type":"noul","noul":1.0})),
            ] {
                let mut optional_wire = wire.clone();
                for id in [
                    "user.objective_relation",
                    "user.feedback_kind",
                    "user.feedback_target",
                ] {
                    match &invalid {
                        Some(answer) => {
                            optional_wire["answers"][id] = answer.clone();
                        }
                        None => {
                            optional_wire["answers"].as_object_mut().unwrap().remove(id);
                        }
                    }
                }
                let adapted = decode_response(&optional_wire, &req.contract).unwrap();
                let parsed = astra_services::parse_work_admission_classification(
                    &judgment,
                    &adapted.full_text,
                    &adapted.model_used,
                    adapted.judgment_provenance,
                )
                .unwrap();
                assert_eq!(
                    parsed.objective_relation,
                    astra_turn_types::ObjectiveRelation::Unknown
                );
                assert!(parsed.feedback.is_none());
                assert_eq!(parsed.into_not_required().unwrap().assessment(), expected);
            }
            wire["answers"]["mutation.read_only"]["noul"] = json!(1.1);
            assert!(decode_response(&wire, &req.contract).is_err());
        }
    }

    #[test]
    fn response_preserves_explicit_cache_usage_lanes() {
        let req = request(&messages(), "jev-1.13.0").unwrap();
        let mut value = good();
        value["usage"]["cache_read_input_tokens"] = json!(7);
        value["usage"]["cache_creation_input_tokens"] = json!(3);
        let result = decode_response(&value, &req.contract).unwrap();
        assert_eq!(result.usage["cached_input_tokens"], 7);
        assert_eq!(result.usage["cache_creation_tokens"], 3);
        assert_eq!(result.usage["total_tokens"], 114);
        let terminal = crate::turn::llm::client::provider_attempt_terminal_from_result(&result);
        assert_eq!(
            terminal.usage_status,
            astra_services::InferenceUsageStatus::ProviderExact
        );
    }

    #[test]
    fn response_rejects_duplicate_keys_before_json_object_conversion() {
        let req = request(&messages(), "jev-1.13.0").unwrap();
        let raw =
            br#"{"model":"jev-1.13.0","answers":{"0":{"type":"noul","noul":0.9,"noul":0.1}}}"#;
        let error = response(raw, &req.contract).unwrap_err();
        assert_eq!(error.kind, ErrorKind::ContractViolation);
    }

    #[test]
    fn response_rejects_discrete_answers_for_every_native_primitive() {
        for (question, answer) in [
            (
                json!({"type":"noul","instructions":"Is this supported?"}),
                json!({"type":"discrete_noul","decision":"yes"}),
            ),
            (
                json!({"type":"choice","instructions":"Which?","criteria":{"a":"A"},"optional":false}),
                json!({"type":"discrete_choice","option":"a"}),
            ),
            (
                json!({"type":"score","instructions":"How much?","criteria":["low","high"]}),
                json!({"type":"discrete_score","level":1}),
            ),
        ] {
            let request: JudgmentRequest = serde_json::from_value(json!({"schema_version":JUDGMENT_SCHEMA_VERSION,"state":{"evidence":"bounded"},"questions":{"q":question}})).unwrap();
            let response = json!({"model":"jev-1.13.0","answers":{"q":answer}});
            let error = decode_response(&response, &request).unwrap_err();
            assert_eq!(error.kind, ErrorKind::ContractViolation);
        }
    }

    #[test]
    fn missing_or_invalid_usage_does_not_discard_valid_judgments() {
        let req = request(&messages(), "jev-1.13.0").unwrap();
        for usage in [
            Value::Null,
            json!({}),
            json!({"input_tokens": -1, "output_tokens": "unknown"}),
        ] {
            let mut body = good();
            body["usage"] = usage;
            let result = decode_response(&body, &req.contract).unwrap();
            assert!(result.usage.is_empty());
            let decoded: JudgmentResponse = serde_json::from_str(&result.full_text).unwrap();
            assert_eq!(decoded.answers["0"].native_noul_probability(), Some(0.9));
        }
        let mut body = good();
        body.as_object_mut().unwrap().remove("usage");
        assert!(
            decode_response(&body, &req.contract)
                .unwrap()
                .usage
                .is_empty()
        );
        body["usage"] = json!({"input_tokens": 0, "output_tokens": "invalid"});
        let result = decode_response(&body, &req.contract).unwrap();
        assert_eq!(result.usage["input_tokens"], 0);
        assert!(result.usage_presence.fresh_input_tokens);
        assert!(!result.usage_presence.output_tokens);
        assert!(result.usage_presence.output_invalid);
        assert!(!result.usage_presence.input_invalid);
        assert_eq!(
            result.usage,
            serde_json::from_value::<serde_json::Map<String, Value>>(json!({"input_tokens": 0}))
                .unwrap()
        );
        body["usage"] = json!({"input_tokens":-1,"output_tokens":7});
        let result = decode_response(&body, &req.contract).unwrap();
        assert!(result.usage_presence.input_invalid);
        assert!(!result.usage_presence.output_invalid);
        assert_eq!(
            result.usage,
            serde_json::from_value::<serde_json::Map<String, Value>>(json!({"output_tokens":7}))
                .unwrap()
        );
    }

    #[tokio::test]
    async fn provider_deadline_is_typed_and_does_not_wait_for_the_slow_response() {
        let (base, _) = mock(200, good(), std::time::Duration::from_secs(2)).await;
        let c = client(base, "mock-key".into());
        let messages = messages();
        let scope = scope();
        let started = Instant::now();
        let error = c
            .complete(crate::memory_hooks::MemoryInferenceRequest {
                purpose: astra_turn_types::InferencePurpose::MemoryRetrievalRerank,
                invocation_scope: &scope,
                messages: &messages,
                max_output_tokens: 50,
                temperature: 0.0,
                deadline: std::time::Duration::from_millis(50),
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::ProviderDeadline);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    async fn mock(
        status: u16,
        body: Value,
        delay: std::time::Duration,
    ) -> (String, Arc<Mutex<Vec<Value>>>) {
        use axum::{Json, Router, http::StatusCode, routing::post};
        let seen = Arc::new(Mutex::new(Vec::new()));
        let captured = seen.clone();
        let handler = move |Json(input): Json<Value>| {
            let body = body.clone();
            let captured = captured.clone();
            async move {
                captured.lock().unwrap().push(input);
                tokio::time::sleep(delay).await;
                (StatusCode::from_u16(status).unwrap(), Json(body))
            }
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/v1/systemone", post(handler)),
            )
            .await
            .unwrap();
        });
        (base, seen)
    }
    #[tokio::test]
    async fn public_memory_paths_batch_and_normalize() {
        let (base,seen)=mock(200,json!({"model":"jev-1.13.0","answers":{"0":{"type":"noul","noul":0.9},"1":{"type":"noul","noul":0.1}},"usage":{"input_tokens":123,"output_tokens":8}}),std::time::Duration::ZERO).await;
        let c = client(base, "mock-key".into());
        let items = vec!["Prefer cargo test".into(), "I like coffee".into()];
        let report = crate::memory_hooks::relevance::select_memories(
            Some(&c),
            Some(&scope()),
            "Run Rust tests",
            &items,
            false,
        )
        .await;
        assert_eq!(report.selected_indices(), vec![0]);
        assert_eq!(
            report.method,
            astra_turn_types::MemorySelectionMethod::Model
        );
        assert_eq!(report.candidates[0].probability_bps, Some(9000));
        assert_eq!(report.candidates[1].probability_bps, Some(1000));
        assert!(report.is_valid());
        assert_eq!(
            select_dismissed_memory_indices(&c, &scope(), "The first lesson is wrong", &items)
                .await,
            vec![0]
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0]["questions"].as_object().unwrap().len(), 2);
        assert!(seen[0].get("messages").is_none());
        assert!(seen[0].get("tools").is_none());
        assert!(
            seen[1]["state"]["policy"]
                .as_str()
                .unwrap()
                .contains("explicitly rejects")
        );
    }
    #[tokio::test]
    async fn unavailable_service_keeps_existing_fallback() {
        for (status, body) in [
            (401, json!({"error":"unauthorized"})),
            (403, json!({"error":"forbidden"})),
            (503, json!({"error":"unavailable"})),
            (429, json!({"error":"limited"})),
            (529, json!({"error":"busy"})),
            (200, json!({"answers":null})),
        ] {
            let (base, _) = mock(status, body, std::time::Duration::ZERO).await;
            let c = client(base, "mock-key".into());
            let items = vec!["Prefer cargo test for Rust changes".into()];
            assert!(
                select_dismissed_memory_indices(&c, &scope(), "This is wrong", &items)
                    .await
                    .is_empty()
            );
            assert_eq!(
                filter_memories(&c, &scope(), "Rust cargo test", &items).await,
                items
            );
        }
    }
    #[tokio::test]
    async fn empty_key_and_extraction_are_rejected_before_dispatch() {
        let (base, seen) = mock(200, good(), std::time::Duration::ZERO).await;
        let c = client(base.clone(), String::new());
        let items = vec!["lesson".into()];
        assert!(
            select_dismissed_memory_indices(&c, &scope(), "wrong", &items)
                .await
                .is_empty()
        );
        let c = client(base, "mock-key".into());
        let m = messages();
        let scope = scope();
        let error = c
            .complete(crate::memory_hooks::MemoryInferenceRequest {
                purpose: astra_turn_types::InferencePurpose::MemoryExtraction,
                invocation_scope: &scope,
                messages: &m,
                max_output_tokens: 50,
                temperature: 0.0,
                deadline: std::time::Duration::from_secs(3),
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert!(seen.lock().unwrap().is_empty());
    }
}
