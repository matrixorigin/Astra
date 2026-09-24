//! TypeSafe System One protocol adapter. Business questions and policies belong to callers.
use super::client::LlmCallResult;
use astra_core::{ClassifiedError, ErrorKind};
use astra_turn_types::{
    JUDGMENT_SCHEMA_VERSION, JudgmentAnswer, JudgmentRequest, JudgmentResponse,
    judgment_request_from_messages, parse_unique_judgment_json,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Instant;

fn invalid(message: &'static str) -> ClassifiedError {
    ClassifiedError::new(ErrorKind::ContractViolation, message)
}

pub(super) fn request(messages: &[Value], model: &str) -> Result<Value, ClassifiedError> {
    let judgment: JudgmentRequest =
        judgment_request_from_messages(messages).map_err(|error| match error {
            astra_turn_types::JudgmentCodecError::Json(_) => {
                invalid("Invalid typed judgment schema")
            }
            astra_turn_types::JudgmentCodecError::Invalid(reason) => invalid(reason),
        })?;
    Ok(json!({"model": model, "state": judgment.state, "questions": judgment.questions}))
}

#[derive(Deserialize)]
struct Response {
    model: String,
    answers: BTreeMap<String, JudgmentAnswer>,
    #[serde(default)]
    usage: Option<Value>,
}

pub(super) fn response(
    raw: &[u8],
    request: &Value,
    started: Instant,
) -> Result<LlmCallResult, ClassifiedError> {
    let value = parse_unique_judgment_json(raw)
        .map_err(|_| invalid("Malformed TypeSafe judgment response"))?;
    let response: Response = serde_json::from_value(value)
        .map_err(|_| invalid("Malformed TypeSafe judgment response"))?;
    let judgment_request = JudgmentRequest {
        schema_version: JUDGMENT_SCHEMA_VERSION,
        state: request
            .get("state")
            .cloned()
            .ok_or_else(|| invalid("Missing TypeSafe state"))?,
        questions: serde_json::from_value(
            request
                .get("questions")
                .cloned()
                .ok_or_else(|| invalid("Missing TypeSafe questions"))?,
        )
        .map_err(|_| invalid("Invalid TypeSafe questions"))?,
    };
    let judgment = JudgmentResponse {
        schema_version: JUDGMENT_SCHEMA_VERSION,
        model: response.model.clone(),
        answers: response.answers,
    };
    judgment
        .validate_for_provenance(
            &judgment_request,
            astra_turn_types::JudgmentResponseProvenance::ProviderProbability,
        )
        .map_err(invalid)?;
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
        duration_ms: started.elapsed().as_millis() as u64,
        finish_reason: Some("stop".into()),
        ..LlmCallResult::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_hooks::relevance::{filter_memories, select_dismissed_memory_indices};
    use crate::memory_hooks::{DirectMemoryInferenceClient, MemoryInferencePort};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

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

    fn decode_response(value: &Value, request: &Value) -> Result<LlmCallResult, ClassifiedError> {
        response(&serde_json::to_vec(value).unwrap(), request, Instant::now())
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
            assert!(decode_response(&v, &req).is_err());
        }
        let result = decode_response(&good(), &req).unwrap();
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
    fn response_preserves_explicit_cache_usage_lanes() {
        let req = request(&messages(), "jev-1.13.0").unwrap();
        let mut value = good();
        value["usage"]["cache_read_input_tokens"] = json!(7);
        value["usage"]["cache_creation_input_tokens"] = json!(3);
        let result = decode_response(&value, &req).unwrap();
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
        let error = response(raw, &req, Instant::now()).unwrap_err();
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
                json!({"type":"choice","instructions":"Which?","criteria":{"a":"A"}}),
                json!({"type":"discrete_choice","option":"a"}),
            ),
            (
                json!({"type":"score","instructions":"How much?","criteria":["low","high"]}),
                json!({"type":"discrete_score","level":1}),
            ),
        ] {
            let request = json!({"state":{"evidence":"bounded"},"questions":{"q":question}});
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
            let result = decode_response(&body, &req).unwrap();
            assert!(result.usage.is_empty());
            let decoded: JudgmentResponse = serde_json::from_str(&result.full_text).unwrap();
            assert_eq!(decoded.answers["0"].native_noul_probability(), Some(0.9));
        }
        let mut body = good();
        body.as_object_mut().unwrap().remove("usage");
        assert!(decode_response(&body, &req).unwrap().usage.is_empty());
        body["usage"] = json!({"input_tokens": 0, "output_tokens": "invalid"});
        let result = decode_response(&body, &req).unwrap();
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
        let result = decode_response(&body, &req).unwrap();
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
