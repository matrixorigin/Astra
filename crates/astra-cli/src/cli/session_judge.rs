//! One auxiliary judgment, using the canonical Server session and inference owners.

use astra_thin_client::{
    CompletionOperation, CompletionRequest, SessionCreateRequest, ThinClient, ThinClientError,
};
use astra_turn_types::{JudgmentRequest, judgment_messages, normalize_judgment_response};
use serde_json::{Value, json};

/// The command's model selector uses the same catalog resolver as chat, with
/// the typed judgment purpose retained on every catalog page.
pub(crate) async fn execute_for_model(
    api: &ThinClient,
    token: &str,
    model: &str,
    message: &str,
    timeout_seconds: u64,
) -> Result<Value, String> {
    let selection = super::session::session_runtime::resolve_server_model_selection(
        api,
        token,
        model,
        astra_core::model_wire::purpose::ModelCatalogPurpose::TypedJudgment,
    )
    .await?;
    Ok(execute(api, token, &selection.offering_id, message, timeout_seconds).await)
}

pub(crate) async fn execute(
    api: &ThinClient,
    token: &str,
    offering_id: &str,
    message: &str,
    timeout_seconds: u64,
) -> Value {
    let judgment: JudgmentRequest = match serde_json::from_str(message) {
        Ok(request) => request,
        Err(error) => {
            return json!({"ok":false,"stage":"validate_request","error":error.to_string()});
        }
    };
    if let Err(error) = judgment.validate() {
        return json!({"ok":false,"stage":"validate_request","error":error});
    }
    let max_tokens = match u32::try_from(judgment.output_token_budget()) {
        Ok(budget) => budget,
        Err(_) => {
            return json!({"ok":false,"stage":"validate_request","error":"judgment output budget exceeds transport limits"});
        }
    };
    // A distinct real session keeps evaluation usage out of the measured agent
    // session. There is no agent run, local workspace, tool set, or chat turn.
    let session = match api
        .create_session(
            Some(token),
            &SessionCreateRequest {
                title: Some("Evidence judgment".into()),
                metadata: Some(serde_json::Map::from_iter([
                    ("purpose".into(), json!("verification_judge")),
                    ("source".into(), json!("cli_session_judge")),
                ])),
                ..Default::default()
            },
        )
        .await
    {
        Ok(session) => session,
        Err(error) => {
            return json!({"ok": false, "stage": "create_session", "error": error.to_string()});
        }
    };
    let Some(session_id) = session
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
    else {
        return json!({"ok": false, "stage": "create_session", "error": "Server omitted a valid session_id"});
    };
    // Every invocation (including a quorum vote) has its own
    // session, so these coordinates identify exactly one auxiliary operation.
    let mut request = CompletionRequest::new(
        CompletionOperation::VerificationJudge,
        session_id,
        1,
        1,
        1,
        judgment_messages(&judgment),
    )
    .with_offering_id(offering_id)
    .with_timeout(std::time::Duration::from_secs(timeout_seconds));
    request.max_tokens = max_tokens;
    let (mut result, close) = match api.post_completions(token, &request).await {
        Ok(response) => {
            let text = response.first_text();
            let finish_reason = response
                .choices
                .first()
                .map(|choice| choice.finish_reason.as_str());
            let normalized = if finish_reason == Some("stop") {
                text.map(|text| {
                    normalize_judgment_response(
                        &judgment,
                        text,
                        &response.model,
                        response.judgment_provenance,
                    )
                })
            } else {
                None
            };
            let error = if text.is_none() {
                Some("completion omitted text".to_string())
            } else if finish_reason != Some("stop") {
                Some(
                    "completion did not finish normally; incomplete judgments cannot be scored"
                        .to_string(),
                )
            } else {
                normalized
                    .as_ref()
                    .and_then(|result| result.as_ref().err())
                    .map(ToString::to_string)
            };
            let (answers, provenance) = match normalized {
                Some(Ok(normalized)) => (Some(normalized.response), Some(normalized.provenance)),
                _ => (None, None),
            };
            (
                json!({
                    "ok": error.is_none(), "stage": "completion", "session_id": session_id,
                    "completion_id": response.id, "offering_id": response.offering_id,
                    "model": response.model, "text": text, "usage": response.usage,
                    "judgment": answers, "provenance": provenance,
                    "finish_reason": finish_reason, "error": error,
                }),
                true,
            )
        }
        Err(error) => {
            // A client-error rejection is explicit. A gateway, transport or
            // decode failure can leave delivery uncertain: retain identity
            // without retrying or treating closure as inference cancellation.
            let close =
                matches!(&error, ThinClientError::Api { status, .. } if status.is_client_error());
            (
                json!({"ok": false, "stage": "completion", "session_id": session_id,
                "offering_id": offering_id, "error": error.to_string(),
                "delivery_uncertain": !close}),
                close,
            )
        }
    };
    if close {
        match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            api.post_session_close_text(token, session_id),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => result["cleanup_warning"] = json!(error.to_string()),
            Err(_) => result["cleanup_warning"] = json!("session close timed out after 10s"),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    const SESSION: &str = "6bca9f9c-6d18-4579-bce1-2b45f573a098";

    #[tokio::test]
    async fn session_judge_resolves_typesafe_through_typed_catalog_before_execution() {
        let native =
            json!({"schema_version":1,"model":"jev","answers":{"0":{"type":"noul","noul":0.9}}})
                .to_string();
        let server = session_server(ResponseTemplate::new(200).set_body_json(json!({
            "id":"completion-1", "object":"chat.completion", "offering_id":"offering-1", "model":"jev",
            "judgment_provenance":"provider_probability",
            "choices":[{"index":0,"message":{"role":"assistant","content":native},"finish_reason":"stop"}]
        })), Some(200)).await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(wiremock::matchers::query_param("purpose", "typed_judgment"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [{"offering_id":"offering-1","access_id":"self-hosted",
                    "access_kind":"self_hosted","access_label":"Self-hosted",
                    "execution_placement":"server","name":"jev","provider":"typesafe",
                    "description":null,"is_active":true,"context_window":64000,
                    "max_completion_tokens":512,"architecture":null,"thinking_capability":null}],
                "total":1,"limit":50,"next_cursor":null,"catalog_revision":"judgment-only"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let api = ThinClient::new(&server.uri(), None).unwrap();
        let result = execute_for_model(&api, "test-token", "jev", &judgment_input(), 37)
            .await
            .expect("CLI judge model selection");
        assert_eq!(result["ok"], true, "{result}");
        let requests = server.received_requests().await.unwrap();
        let completion = requests
            .iter()
            .find(|request| request.url.path() == "/v1/chat/completions")
            .unwrap();
        let body: Value = serde_json::from_slice(&completion.body).unwrap();
        assert_eq!(body["model_selection"]["offering_id"], "offering-1");
        assert!(
            !requests
                .iter()
                .any(|request| request.url.path() == "/chat/stream")
        );
    }

    async fn session_server(completion: ResponseTemplate, close_status: Option<u16>) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sessions"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"session_id": SESSION})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(completion)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/sessions/{SESSION}/close")))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(
                ResponseTemplate::new(close_status.unwrap_or(200)).set_body_json(json!({})),
            )
            .expect(u64::from(close_status.is_some()))
            .mount(&server)
            .await;
        server
    }

    fn completion_with_finish_reason(reason: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "completion-1", "object": "chat.completion", "offering_id": "offering-1", "model": "judge",
            "judgment_provenance": "discrete_decision",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "{\"true\":[],\"uncertain\":[]}"}, "finish_reason": reason}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110},
        }))
    }

    fn judgment_input() -> String {
        json!({"schema_version":1,"state":{"evidence":"quoted evidence"},"questions":{"0":{"type":"noul","instructions":"Evidence supports the criterion."}}}).to_string()
    }

    fn completion() -> ResponseTemplate {
        completion_with_finish_reason("stop")
    }

    #[tokio::test]
    async fn nonterminal_judgment_is_rejected_even_with_valid_decisions() {
        for reason in ["length", "content_filter", "unknown"] {
            let server = session_server(completion_with_finish_reason(reason), Some(200)).await;
            let api = ThinClient::new(&server.uri(), None).unwrap();
            let result = execute(&api, "test-token", "offering-1", &judgment_input(), 37).await;
            assert_eq!(result["ok"], false);
            assert_eq!(result["finish_reason"], reason);
            assert_eq!(result["completion_id"], "completion-1");
            assert!(result["judgment"].is_null());
        }
    }

    #[tokio::test]
    async fn gateway_failure_retains_uncertain_delivery_without_closing_session() {
        let server = session_server(ResponseTemplate::new(504), None).await;
        let api = ThinClient::new(&server.uri(), None).unwrap();
        let result = execute(&api, "test-token", "offering-1", &judgment_input(), 37).await;
        assert_eq!(result["ok"], false);
        assert_eq!(result["session_id"], SESSION);
        assert_eq!(result["delivery_uncertain"], true);
    }

    #[tokio::test]
    async fn judgment_uses_only_governed_auxiliary_inference_and_closes_owned_session() {
        let server = session_server(completion(), Some(200)).await;
        let api = ThinClient::new(&server.uri(), None).unwrap();
        let result = execute(&api, "test-token", "offering-1", &judgment_input(), 37).await;
        assert_eq!(result["ok"], true);
        assert_eq!(result["session_id"], SESSION);
        assert_eq!(result["usage"]["total_tokens"], 110);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            3,
            "no chat/run/tool HTTP requests permitted"
        );
        let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(body["operation"], "verification_judge");
        assert_eq!(body["session_id"], SESSION);
        assert_eq!(body["model_selection"]["offering_id"], "offering-1");
        let judgment: JudgmentRequest = serde_json::from_str(&judgment_input()).unwrap();
        assert_eq!(body["max_tokens"], judgment.output_token_budget());
        assert_eq!(body["messages"], json!(judgment_messages(&judgment)));
        assert_eq!(result["provenance"], "discrete_decision");
        assert_eq!(result["judgment"]["answers"]["0"]["noul"], 0.0);
        assert_eq!(body["timeout_ms"], 37_000);
        assert!(body.get("tools").is_none());
        let metadata: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(!metadata.to_string().contains("quoted evidence"));
    }

    #[tokio::test]
    async fn session_close_failure_preserves_completed_judgment_with_warning() {
        let server = session_server(completion(), Some(503)).await;
        let api = ThinClient::new(&server.uri(), None).unwrap();
        let result = execute(&api, "test-token", "offering-1", &judgment_input(), 37).await;
        assert_eq!(result["ok"], true);
        assert!(result["cleanup_warning"].as_str().unwrap().contains("503"));
    }

    #[tokio::test]
    async fn completion_rejection_preserves_identity_and_does_not_retry() {
        let server = session_server(
            ResponseTemplate::new(429).set_body_string("capacity exhausted"),
            Some(200),
        )
        .await;
        let api = ThinClient::new(&server.uri(), None).unwrap();
        let result = execute(&api, "test-token", "offering-1", &judgment_input(), 37).await;
        assert_eq!(result["ok"], false);
        assert_eq!(result["session_id"], SESSION);
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("capacity exhausted")
        );
    }

    #[tokio::test]
    async fn malformed_completion_keeps_session_for_uncertain_delivery_diagnosis() {
        let server = session_server(
            ResponseTemplate::new(200).set_body_string("broken transport body"),
            None,
        )
        .await;
        let api = ThinClient::new(&server.uri(), None).unwrap();
        let result = execute(&api, "test-token", "offering-1", &judgment_input(), 37).await;
        assert_eq!(result["ok"], false);
        assert_eq!(result["session_id"], SESSION);
        assert_eq!(result["delivery_uncertain"], true);
    }
    #[tokio::test]
    async fn invalid_typed_request_does_not_create_session() {
        let server = MockServer::start().await;
        let api = ThinClient::new(&server.uri(), None).unwrap();
        for message in [
            "free form rubric",
            r#"{"schema_version":1,"state":null,"questions":{}}"#,
        ] {
            let result = execute(&api, "test-token", "offering-1", message, 37).await;
            assert_eq!(result["ok"], false);
            assert_eq!(result["stage"], "validate_request");
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn malformed_decisions_close_completed_session_without_retry() {
        let body = json!({"id":"completion-1","object":"chat.completion","offering_id":"offering-1","model":"judge","judgment_provenance":"discrete_decision","choices":[{"index":0,"message":{"role":"assistant","content":"{\"true\":[\"unknown\"],\"uncertain\":[]}"},"finish_reason":"stop"}]});
        let server =
            session_server(ResponseTemplate::new(200).set_body_json(body), Some(200)).await;
        let api = ThinClient::new(&server.uri(), None).unwrap();
        let result = execute(&api, "test-token", "offering-1", &judgment_input(), 37).await;
        assert_eq!(result["ok"], false);
        assert_eq!(result["finish_reason"], "stop");
        assert!(result["judgment"].is_null());
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }
    #[tokio::test]
    async fn judgment_source_and_identity_come_from_server_execution_metadata() {
        for (provenance, valid) in [
            (Some("provider_probability"), true),
            (Some("discrete_decision"), false),
            (None, false),
        ] {
            let text = json!({"schema_version":1,"model":"untrusted-answer-model","answers":{"0":{"type":"noul","noul":0.93}}}).to_string();
            let body = json!({"id":"completion-1","object":"chat.completion","offering_id":"offering-1","model":"native-judge","judgment_provenance":provenance,"choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}]});
            let server =
                session_server(ResponseTemplate::new(200).set_body_json(body), Some(200)).await;
            let api = ThinClient::new(&server.uri(), None).unwrap();
            let result = execute(&api, "test-token", "offering-1", &judgment_input(), 37).await;
            assert_eq!(result["ok"], valid, "{result}");
            if valid {
                assert_eq!(result["provenance"], "provider_probability");
                assert_eq!(result["judgment"]["model"], "native-judge");
                assert_eq!(result["judgment"]["answers"]["0"]["noul"], 0.93);
            } else {
                assert!(result["judgment"].is_null());
            }
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                3,
                "one completion and owned session create/close; no repair inference"
            );
        }
    }
}
