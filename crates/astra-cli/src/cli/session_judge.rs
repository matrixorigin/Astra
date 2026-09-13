//! One auxiliary judgment, using the canonical Server session and inference owners.

use astra_thin_client::{
    CompletionOperation, CompletionRequest, SessionCreateRequest, ThinClient, ThinClientError,
};
use serde_json::{Value, json};

pub(crate) async fn execute(
    api: &ThinClient,
    token: &str,
    offering_id: &str,
    message: &str,
    timeout_seconds: u64,
) -> Value {
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
    // Every invocation (including a format repair or quorum vote) has its own
    // session, so these coordinates identify exactly one auxiliary operation.
    let mut request = CompletionRequest::new(
        CompletionOperation::VerificationJudge,
        session_id,
        1,
        1,
        1,
        vec![json!({"role": "user", "content": message})],
    )
    .with_offering_id(offering_id)
    .with_timeout(std::time::Duration::from_secs(timeout_seconds));
    request.max_tokens = 2_048;
    let (mut result, close) = match api.post_completions(token, &request).await {
        Ok(response) => {
            let text = response.first_text();
            let finish_reason = response
                .choices
                .first()
                .map(|choice| choice.finish_reason.as_str());
            let error = if text.is_none() {
                Some("completion omitted text")
            } else if finish_reason != Some("stop") {
                Some("completion did not finish normally; incomplete judgments cannot be scored")
            } else {
                None
            };
            (
                json!({
                    "ok": error.is_none(), "stage": "completion", "session_id": session_id,
                    "completion_id": response.id, "offering_id": response.offering_id,
                    "model": response.model, "text": text, "usage": response.usage,
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
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "Evidence is insufficient.\nSCORE: 0.0"}, "finish_reason": reason}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110},
        }))
    }

    fn completion() -> ResponseTemplate {
        completion_with_finish_reason("stop")
    }

    #[tokio::test]
    async fn nonterminal_judgment_is_rejected_even_with_a_valid_score_line() {
        for reason in ["length", "content_filter", "unknown"] {
            let server = session_server(completion_with_finish_reason(reason), Some(200)).await;
            let api = ThinClient::new(&server.uri(), None).unwrap();
            let result = execute(&api, "test-token", "offering-1", "evidence", 37).await;
            assert_eq!(result["ok"], false);
            assert_eq!(result["finish_reason"], reason);
            assert_eq!(result["completion_id"], "completion-1");
            assert!(result["text"].as_str().unwrap().contains("SCORE: 0.0"));
        }
    }

    #[tokio::test]
    async fn gateway_failure_retains_uncertain_delivery_without_closing_session() {
        let server = session_server(ResponseTemplate::new(504), None).await;
        let api = ThinClient::new(&server.uri(), None).unwrap();
        let result = execute(&api, "test-token", "offering-1", "evidence", 37).await;
        assert_eq!(result["ok"], false);
        assert_eq!(result["session_id"], SESSION);
        assert_eq!(result["delivery_uncertain"], true);
    }

    #[tokio::test]
    async fn judgment_uses_only_governed_auxiliary_inference_and_closes_owned_session() {
        let server = session_server(completion(), Some(200)).await;
        let api = ThinClient::new(&server.uri(), None).unwrap();
        let result = execute(&api, "test-token", "offering-1", "quoted evidence", 37).await;
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
        assert_eq!(body["timeout_ms"], 37_000);
        assert!(body.get("tools").is_none());
        let metadata: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(!metadata.to_string().contains("quoted evidence"));
    }

    #[tokio::test]
    async fn session_close_failure_preserves_completed_judgment_with_warning() {
        let server = session_server(completion(), Some(503)).await;
        let api = ThinClient::new(&server.uri(), None).unwrap();
        let result = execute(&api, "test-token", "offering-1", "evidence", 37).await;
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
        let result = execute(&api, "test-token", "offering-1", "evidence", 37).await;
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
        let result = execute(&api, "test-token", "offering-1", "evidence", 37).await;
        assert_eq!(result["ok"], false);
        assert_eq!(result["session_id"], SESSION);
        assert_eq!(result["delivery_uncertain"], true);
    }
}
