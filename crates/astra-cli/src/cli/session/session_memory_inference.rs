//! Typed memory-inference adapter for Astra Server.
//!
//! This is intentionally distinct from the runtime's direct-provider adapter:
//! Astra Server requires typed inference purpose for admission and attribution,
//! while ordinary provider payloads must not receive Astra-only fields.

use astra_core::{ClassifiedError, ErrorKind};
use astra_runtime::memory_hooks::{MemoryInferencePort, MemoryInferenceRequest};
use astra_thin_client::ThinClientError;

pub(crate) struct CliServerMemoryInferenceClient {
    api: astra_thin_client::ThinClient,
    token: String,
    model_name: String,
}

impl CliServerMemoryInferenceClient {
    pub(crate) fn new(
        api: astra_thin_client::ThinClient,
        token: impl Into<String>,
        model_name: impl Into<String>,
    ) -> Self {
        Self {
            api,
            token: token.into(),
            model_name: model_name.into(),
        }
    }
}

impl std::fmt::Debug for CliServerMemoryInferenceClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliServerMemoryInferenceClient")
            .field("model_name", &self.model_name)
            .field("credential_present", &!self.token.is_empty())
            .finish()
    }
}

#[derive(serde::Deserialize)]
struct CompletionEnvelope {
    choices: Vec<CompletionChoice>,
}

#[derive(serde::Deserialize)]
struct CompletionChoice {
    message: CompletionMessage,
}

#[derive(serde::Deserialize)]
struct CompletionMessage {
    content: String,
}

#[async_trait::async_trait]
impl MemoryInferencePort for CliServerMemoryInferenceClient {
    fn model_name(&self) -> &str {
        &self.model_name
    }

    async fn complete(
        &self,
        request: MemoryInferenceRequest<'_>,
    ) -> Result<String, ClassifiedError> {
        let body = serde_json::json!({
            "purpose": request.purpose,
            "model": self.model_name,
            "messages": request.messages,
            "max_tokens": request.max_output_tokens,
            "temperature": request.temperature,
        });
        let response = tokio::time::timeout(
            request.deadline,
            self.api.post_completions(&self.token, &body),
        )
        .await
        .map_err(|_| {
            ClassifiedError::new(
                ErrorKind::StreamIdle,
                "Astra Server memory inference exceeded its deadline",
            )
        })?
        .map_err(classify_thin_client_error)?;
        let envelope = serde_json::from_value::<CompletionEnvelope>(response).map_err(|_| {
            ClassifiedError::new(
                ErrorKind::ServerError,
                "Astra Server returned a malformed completion response",
            )
        })?;
        envelope
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message.content)
            .ok_or_else(|| {
                ClassifiedError::new(
                    ErrorKind::ServerError,
                    "Astra Server returned a completion without choices",
                )
            })
    }
}

fn classify_thin_client_error(error: ThinClientError) -> ClassifiedError {
    let (kind, message) = match error {
        ThinClientError::InvalidBaseUrl(_) | ThinClientError::InvalidInput(_) => (
            ErrorKind::InvalidRequest,
            "Astra Server memory inference configuration is invalid",
        ),
        ThinClientError::InvalidAuthHeader => (
            ErrorKind::Auth,
            "Astra Server memory inference credentials are invalid",
        ),
        ThinClientError::Http(error) if error.is_timeout() => (
            ErrorKind::StreamIdle,
            "Astra Server memory inference request timed out",
        ),
        ThinClientError::Http(error) if error.is_connect() => (
            ErrorKind::Network,
            "Astra Server memory inference is unreachable",
        ),
        ThinClientError::Http(_) => (
            ErrorKind::StreamTransport,
            "Astra Server memory inference transport failed",
        ),
        ThinClientError::Api { status, .. } => {
            let kind = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                ErrorKind::RateLimit
            } else if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                ErrorKind::Auth
            } else if status.is_server_error() {
                ErrorKind::ServerError
            } else if status.is_client_error() {
                ErrorKind::InvalidRequest
            } else {
                ErrorKind::Unknown
            };
            return ClassifiedError::new(
                kind,
                format!("Astra Server memory inference returned HTTP {status}"),
            );
        }
        ThinClientError::Json(_)
        | ThinClientError::SseParse(_)
        | ThinClientError::InvalidSseJson(_) => (
            ErrorKind::ServerError,
            "Astra Server returned an invalid memory inference response",
        ),
    };
    ClassifiedError::new(kind, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_runtime::memory_hooks::MemoryInferencePort;
    use astra_turn_types::InferencePurpose;
    use axum::{Json, Router, routing::post};
    use std::time::Duration;

    #[tokio::test]
    async fn server_memory_inference_preserves_typed_purpose_on_the_wire() {
        let (request_tx, request_rx) = tokio::sync::oneshot::channel();
        let request_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(request_tx)));
        let app = Router::new().route(
            "/v1/chat/completions",
            post({
                let request_tx = request_tx.clone();
                move |Json(body): Json<serde_json::Value>| {
                    let request_tx = request_tx.clone();
                    async move {
                        if let Some(tx) = request_tx.lock().expect("request lock").take() {
                            tx.send(body).expect("capture request body");
                        }
                        Json(serde_json::json!({
                            "choices": [{"message": {"content": "[0]"}}]
                        }))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve test request");
        });
        let origin = format!("http://{address}");
        let api = astra_thin_client::ThinClient::new(&origin, None).expect("test client");
        let client = CliServerMemoryInferenceClient::new(api, "token", "memory-offering");
        let messages = [serde_json::json!({"role": "user", "content": "rank"})];

        let output = client
            .complete(MemoryInferenceRequest {
                purpose: InferencePurpose::MemoryRetrievalRerank,
                messages: &messages,
                max_output_tokens: 50,
                temperature: 0.0,
                deadline: Duration::from_secs(2),
            })
            .await
            .expect("completion response");
        let body = request_rx.await.expect("captured request");

        assert_eq!(output, "[0]");
        assert_eq!(body["purpose"], "memory_retrieval_rerank");
        assert_eq!(body["model"], "memory-offering");
        assert_eq!(body["messages"], serde_json::json!(messages));
        server.abort();
        assert!(
            server
                .await
                .expect_err("test server should be cancelled")
                .is_cancelled()
        );
    }

    #[test]
    fn server_status_errors_are_classified_without_parsing_error_prose() {
        let error = classify_thin_client_error(ThinClientError::Api {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            body: "arbitrary provider prose".to_string(),
        });
        assert_eq!(error.kind, ErrorKind::RateLimit);
        assert!(!error.message.contains("arbitrary provider prose"));
    }
}
