//! Typed memory-inference adapter for Astra Server.
//!
//! This is intentionally distinct from the runtime's direct-provider adapter:
//! Astra Server requires typed inference purpose for admission and attribution,
//! while ordinary provider payloads must not receive Astra-only fields.

use astra_core::{ClassifiedError, ErrorKind};
use astra_runtime::memory_hooks::{MemoryInferencePort, MemoryInferenceRequest};
use astra_thin_client::ThinClientError;

#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemoryInferenceOffering {
    pub offering_id: String,
    pub model_name: String,
    pub thinking_capability: Option<astra_services::models::ThinkingCapability>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryInferenceOfferingsEnvelope {
    offerings: Vec<MemoryInferenceOffering>,
}

pub(crate) async fn fetch_memory_inference_offerings(
    api: &astra_thin_client::ThinClient,
    token: &str,
) -> Result<Vec<MemoryInferenceOffering>, String> {
    let body = api
        .get_authed_path_text(token, astra_thin_client::paths::model_memory())
        .await
        .map_err(|error| format!("memory inference catalog is unavailable: {error}"))?;
    let envelope = serde_json::from_str::<MemoryInferenceOfferingsEnvelope>(&body)
        .map_err(|error| format!("memory inference catalog is malformed: {error}"))?;
    validate_memory_inference_offerings(envelope.offerings)
}

fn validate_memory_inference_offerings(
    offerings: Vec<MemoryInferenceOffering>,
) -> Result<Vec<MemoryInferenceOffering>, String> {
    if offerings.is_empty() {
        return Err("memory inference catalog contains no usable Offering".to_string());
    }
    let mut seen = std::collections::HashSet::new();
    for offering in &offerings {
        astra_services::validate_model_offering_id(&offering.offering_id).map_err(|_| {
            format!(
                "memory inference catalog contains invalid Offering ID {:?}",
                offering.offering_id
            )
        })?;
        if offering.model_name.trim().is_empty() {
            return Err(format!(
                "memory inference Offering {:?} has no display model name",
                offering.offering_id
            ));
        }
        if !seen.insert(offering.offering_id.as_str()) {
            return Err(format!(
                "memory inference catalog repeats Offering {:?}",
                offering.offering_id
            ));
        }
    }
    Ok(offerings)
}

pub(crate) struct CliServerMemoryInferenceClient {
    api: astra_thin_client::ThinClient,
    token: String,
    offering_id: String,
    model_name: String,
}

impl CliServerMemoryInferenceClient {
    pub(crate) fn new(
        api: astra_thin_client::ThinClient,
        token: impl Into<String>,
        offering_id: impl Into<String>,
        model_name: impl Into<String>,
    ) -> Self {
        Self {
            api,
            token: token.into(),
            offering_id: offering_id.into(),
            model_name: model_name.into(),
        }
    }
}

impl std::fmt::Debug for CliServerMemoryInferenceClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliServerMemoryInferenceClient")
            .field("offering_id", &self.offering_id)
            .field("model_name", &self.model_name)
            .field("credential_present", &!self.token.is_empty())
            .finish()
    }
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
        let operation = match request.purpose {
            astra_turn_types::InferencePurpose::MemoryExtraction => {
                astra_thin_client::CompletionOperation::MemoryExtraction
            }
            astra_turn_types::InferencePurpose::MemoryRetrievalRerank => {
                astra_thin_client::CompletionOperation::MemoryRetrievalRerank
            }
            purpose => {
                return Err(ClassifiedError::new(
                    ErrorKind::InvalidRequest,
                    format!("unsupported memory completion purpose {purpose}"),
                ));
            }
        };
        let mut completion = astra_thin_client::CompletionRequest::from_session_scope(
            operation,
            &request.invocation_scope,
            request.messages.to_vec(),
        )
        .map_err(|error| ClassifiedError::new(ErrorKind::InvalidRequest, error))?
        .with_offering_id(&self.offering_id)
        .with_timeout(request.deadline);
        completion.max_tokens = request.max_output_tokens.try_into().map_err(|_| {
            ClassifiedError::new(
                ErrorKind::InvalidRequest,
                "Memory inference output limit exceeds the Server protocol range",
            )
        })?;
        completion.temperature = request.temperature;
        let response = self
            .api
            .post_completions(&self.token, &completion)
            .await
            .map_err(classify_thin_client_error)?;
        response
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
    async fn server_memory_inference_sends_typed_operation_and_session_coordinates() {
        let (request_tx, request_rx) = tokio::sync::oneshot::channel();
        let request_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(request_tx)));
        let app = Router::new().route(
            "/v1/chat/completions",
            post({
                let request_tx = request_tx.clone();
                move |Json(body): Json<astra_thin_client::CompletionRequest>| {
                    let request_tx = request_tx.clone();
                    async move {
                        if let Some(tx) = request_tx.lock().expect("request lock").take() {
                            tx.send(body).expect("capture request body");
                        }
                        Json(serde_json::json!({
                            "id": "memory-response",
                            "object": "chat.completion",
                            "offering_id": "memory-offering",
                            "model": "memory-model",
                            "choices": [{
                                "index": 0,
                                "message": {"role": "assistant", "content": "[0]"},
                                "finish_reason": "stop"
                            }]
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
        let client =
            CliServerMemoryInferenceClient::new(api, "token", "memory-offering", "memory-model");
        let messages = [serde_json::json!({"role": "user", "content": "rank"})];

        let output = client
            .complete(MemoryInferenceRequest {
                purpose: InferencePurpose::MemoryRetrievalRerank,
                invocation_scope: &astra_turn_types::InferenceInvocationScope::Session {
                    session_id: "session-memory".to_string(),
                    turn: 1,
                    round: 0,
                    operation_id: "memory_rerank".to_string(),
                    logical_attempt: 0,
                },
                messages: &messages,
                max_output_tokens: 50,
                temperature: 0.0,
                deadline: Duration::from_secs(2),
            })
            .await
            .expect("completion response");
        let body = request_rx.await.expect("captured request");

        assert_eq!(output, "[0]");
        assert_eq!(
            body.operation,
            astra_thin_client::CompletionOperation::MemoryRetrievalRerank
        );
        assert_eq!(
            body.model_selection,
            Some(astra_turn_types::ModelSelection {
                offering_id: "memory-offering".to_string(),
            })
        );
        assert_eq!(body.session_id, "session-memory");
        assert_eq!(body.turn, 1);
        assert_eq!(body.round, 0);
        assert_eq!(body.logical_attempt, 0);
        assert_eq!(body.messages, messages);
        assert_eq!(
            body.invocation_scope(),
            astra_turn_types::InferenceInvocationScope::Session {
                session_id: "session-memory".to_string(),
                turn: 1,
                round: 0,
                operation_id: "completion_proxy:memory_retrieval_rerank".to_string(),
                logical_attempt: 0,
            }
        );
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

    #[test]
    fn memory_catalog_rejects_the_legacy_parallel_array_shape() {
        let legacy = serde_json::json!({
            "model_name": "display-model",
            "candidate_model_names": ["display-model"],
            "candidate_thinking_capabilities": [null]
        });
        assert!(
            serde_json::from_value::<MemoryInferenceOfferingsEnvelope>(legacy).is_err(),
            "parallel model-name arrays must not remain a second routing contract"
        );
    }

    #[tokio::test]
    async fn memory_catalog_preserves_ordered_typed_offerings() {
        let app = Router::new().route(
            "/models/memory",
            axum::routing::get(|| async {
                Json(serde_json::json!({
                    "offerings": [
                        {
                            "offering_id": "offer-first",
                            "model_name": "first-display",
                            "thinking_capability": null
                        },
                        {
                            "offering_id": "offer-second",
                            "model_name": "second-display",
                            "thinking_capability": "both"
                        }
                    ]
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind catalog server");
        let address = listener.local_addr().expect("catalog server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve catalog request");
        });
        let api = astra_thin_client::ThinClient::new(&format!("http://{address}"), None)
            .expect("catalog client");

        let offerings = fetch_memory_inference_offerings(&api, "token")
            .await
            .expect("valid catalog");

        assert_eq!(
            offerings
                .iter()
                .map(|offering| offering.offering_id.as_str())
                .collect::<Vec<_>>(),
            vec!["offer-first", "offer-second"]
        );
        assert_eq!(offerings[1].model_name, "second-display");
        assert_eq!(
            offerings[1].thinking_capability,
            Some(astra_services::models::ThinkingCapability::Both)
        );
        server.abort();
        assert!(
            server
                .await
                .expect_err("catalog server should be cancelled")
                .is_cancelled()
        );
    }

    #[test]
    fn memory_catalog_rejects_invalid_or_duplicate_offerings_as_a_whole() {
        let invalid = vec![MemoryInferenceOffering {
            offering_id: " bad-id".into(),
            model_name: "display".into(),
            thinking_capability: None,
        }];
        assert!(validate_memory_inference_offerings(invalid).is_err());

        let duplicate = vec![
            MemoryInferenceOffering {
                offering_id: "offer-same".into(),
                model_name: "first".into(),
                thinking_capability: None,
            },
            MemoryInferenceOffering {
                offering_id: "offer-same".into(),
                model_name: "second".into(),
                thinking_capability: None,
            },
        ];
        assert!(validate_memory_inference_offerings(duplicate).is_err());
    }
}
