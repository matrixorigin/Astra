use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use astra_core::{ErrorResponse, error_response_coded};
use astra_services::runs::SelectedModelRequest;
use astra_services::{LlmTokenServiceConfig, ModelGatewayRecord, ModelProtocol};

const MODEL_GATEWAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MODEL_GATEWAY_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Serialize)]
struct ModelResolveRequest<'a> {
    model: &'a str,
    gateway: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelResolveResponse {
    model: String,
    status: String,
    protocol: ModelProtocol,
    invoke: ModelInvokeDescriptor,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelInvokeDescriptor {
    url: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

fn model_gateway_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        astra_core::net::build_internal_http_client(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(MODEL_GATEWAY_CONNECT_TIMEOUT)
                .timeout(MODEL_GATEWAY_REQUEST_TIMEOUT),
            "model gateway client",
        )
    })
}

pub(crate) async fn resolve_model_gateway_invocation(
    gateway: &ModelGatewayRecord,
    selected_model: &SelectedModelRequest,
    authorization: &str,
) -> Result<LlmTokenServiceConfig, (StatusCode, Json<ErrorResponse>)> {
    let selected_gateway = selected_model.gateway.as_deref().ok_or_else(|| {
        error_response_coded(
            StatusCode::BAD_REQUEST,
            "selected_model.gateway is required for model gateway resolve",
            "selected_model_invalid",
        )
    })?;
    if selected_gateway != gateway.id {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "selected_model.gateway does not match resolved model gateway",
            "selected_model_invalid",
        ));
    }

    let _permit =
        crate::capability_endpoint_pool::try_acquire_endpoint_permit(&gateway.resolve_url)
            .map_err(|detail| {
                error_response_coded(
                    StatusCode::TOO_MANY_REQUESTS,
                    detail,
                    "model_gateway_resolve_failed",
                )
            })?;
    let response = model_gateway_http_client()
        .post(&gateway.resolve_url)
        .header(reqwest::header::AUTHORIZATION, authorization)
        .json(&ModelResolveRequest {
            model: &selected_model.model,
            gateway: selected_gateway,
        })
        .send()
        .await
        .map_err(|error| {
            tracing::warn!(
                gateway_id = %gateway.id,
                error = %error,
                "model gateway resolve request failed"
            );
            error_response_coded(
                StatusCode::BAD_GATEWAY,
                "model gateway resolve request failed",
                "model_gateway_resolve_failed",
            )
        })?;

    let status = response.status();
    if !status.is_success() {
        tracing::warn!(
            gateway_id = %gateway.id,
            http_status = %status,
            "model gateway resolve endpoint rejected request"
        );
        return Err(error_response_coded(
            StatusCode::BAD_GATEWAY,
            "model gateway resolve endpoint rejected request",
            "model_gateway_resolve_failed",
        ));
    }

    let descriptor = response
        .json::<ModelResolveResponse>()
        .await
        .map_err(|error| {
            tracing::warn!(
                gateway_id = %gateway.id,
                error = %error,
                "model gateway resolve response was not a valid descriptor"
            );
            error_response_coded(
                StatusCode::BAD_GATEWAY,
                "model gateway resolve response was not a valid descriptor",
                "model_gateway_descriptor_invalid",
            )
        })?;

    validate_descriptor(gateway, selected_model, descriptor)
}

fn validate_descriptor(
    gateway: &ModelGatewayRecord,
    selected_model: &SelectedModelRequest,
    descriptor: ModelResolveResponse,
) -> Result<LlmTokenServiceConfig, (StatusCode, Json<ErrorResponse>)> {
    if descriptor.status != "ready" {
        return Err(error_response_coded(
            StatusCode::BAD_GATEWAY,
            "model gateway did not return a ready descriptor",
            "model_gateway_resolve_failed",
        ));
    }
    if descriptor.protocol != gateway.model_protocol {
        return Err(error_response_coded(
            StatusCode::BAD_GATEWAY,
            "model gateway descriptor protocol does not match registered protocol",
            "model_gateway_descriptor_invalid",
        ));
    }
    if descriptor.model != selected_model.model {
        return Err(error_response_coded(
            StatusCode::BAD_GATEWAY,
            "model gateway descriptor model does not match selected_model.model",
            "model_gateway_descriptor_invalid",
        ));
    }
    validate_invoke_url(&descriptor.invoke.url)?;
    validate_invoke_timeout(descriptor.invoke.timeout_ms)?;
    Ok(LlmTokenServiceConfig {
        url: descriptor.invoke.url,
        timeout_ms: descriptor.invoke.timeout_ms,
    })
}

fn validate_invoke_timeout(
    timeout_ms: Option<u64>,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if timeout_ms == Some(0) {
        return Err(error_response_coded(
            StatusCode::BAD_GATEWAY,
            "model gateway descriptor invoke.timeout_ms must be positive when present",
            "model_gateway_descriptor_invalid",
        ));
    }
    Ok(())
}

fn validate_invoke_url(url: &str) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if url.is_empty() || url.trim() != url || url.chars().any(char::is_control) {
        return Err(error_response_coded(
            StatusCode::BAD_GATEWAY,
            "model gateway descriptor invoke.url must be a non-empty exact string",
            "model_gateway_descriptor_invalid",
        ));
    }
    let parsed = reqwest::Url::parse(url).map_err(|error| {
        error_response_coded(
            StatusCode::BAD_GATEWAY,
            format!("model gateway descriptor invoke.url is invalid: {error}"),
            "model_gateway_descriptor_invalid",
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(error_response_coded(
            StatusCode::BAD_GATEWAY,
            "model gateway descriptor invoke.url must be an absolute http or https URL",
            "model_gateway_descriptor_invalid",
        ));
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(error_response_coded(
            StatusCode::BAD_GATEWAY,
            "model gateway descriptor invoke.url must not contain userinfo, query, or fragment",
            "model_gateway_descriptor_invalid",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn gateway() -> ModelGatewayRecord {
        ModelGatewayRecord {
            id: "gw-1".to_string(),
            resolve_url: "https://gateway.example.com/resolve".to_string(),
            model_protocol: ModelProtocol::OpenAiChatCompletions,
            status: astra_services::ModelGatewayStatus::Active,
            metadata: None,
            created_at: "2026-01-01 00:00:00".to_string(),
            updated_at: "2026-01-01 00:00:00".to_string(),
            disabled_at: None,
        }
    }

    fn selected_model() -> SelectedModelRequest {
        SelectedModelRequest {
            id: None,
            model: "gpt-4.1".to_string(),
            gateway: Some("gw-1".to_string()),
        }
    }

    #[test]
    fn descriptor_accepts_exact_ready_model_and_url() {
        let config = validate_descriptor(
            &gateway(),
            &selected_model(),
            ModelResolveResponse {
                model: "gpt-4.1".to_string(),
                status: "ready".to_string(),
                protocol: ModelProtocol::OpenAiChatCompletions,
                invoke: ModelInvokeDescriptor {
                    url: "https://models.example.com/v1/chat/completions".to_string(),
                    timeout_ms: Some(120_000),
                },
            },
        )
        .expect("descriptor should be accepted");

        assert_eq!(config.url, "https://models.example.com/v1/chat/completions");
        assert_eq!(config.timeout_ms, Some(120_000));
    }

    #[test]
    fn descriptor_rejects_model_mismatch() {
        let err = validate_descriptor(
            &gateway(),
            &selected_model(),
            ModelResolveResponse {
                model: "other".to_string(),
                status: "ready".to_string(),
                protocol: ModelProtocol::OpenAiChatCompletions,
                invoke: ModelInvokeDescriptor {
                    url: "https://models.example.com/v1/chat/completions".to_string(),
                    timeout_ms: None,
                },
            },
        )
        .expect_err("descriptor model must match selected model");

        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
        assert_eq!(
            err.1.error_code.as_deref(),
            Some("model_gateway_descriptor_invalid")
        );
    }

    #[test]
    fn descriptor_rejects_credential_bearing_invoke_url() {
        let err = validate_descriptor(
            &gateway(),
            &selected_model(),
            ModelResolveResponse {
                model: "gpt-4.1".to_string(),
                status: "ready".to_string(),
                protocol: ModelProtocol::OpenAiChatCompletions,
                invoke: ModelInvokeDescriptor {
                    url: "https://models.example.com/v1/chat/completions?token=secret".to_string(),
                    timeout_ms: None,
                },
            },
        )
        .expect_err("invoke URL must not carry query credentials");

        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
        assert_eq!(
            err.1.error_code.as_deref(),
            Some("model_gateway_descriptor_invalid")
        );
    }

    #[test]
    fn descriptor_rejects_zero_invoke_timeout() {
        let err = validate_descriptor(
            &gateway(),
            &selected_model(),
            ModelResolveResponse {
                model: "gpt-4.1".to_string(),
                status: "ready".to_string(),
                protocol: ModelProtocol::OpenAiChatCompletions,
                invoke: ModelInvokeDescriptor {
                    url: "https://models.example.com/v1/chat/completions".to_string(),
                    timeout_ms: Some(0),
                },
            },
        )
        .expect_err("invoke timeout must be positive when present");

        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
        assert_eq!(
            err.1.error_code.as_deref(),
            Some("model_gateway_descriptor_invalid")
        );
    }

    #[tokio::test]
    async fn resolve_model_gateway_invocation_sends_selected_model_and_runtime_auth() {
        use axum::{Router, extract::State, http::HeaderMap, routing::post};
        use std::sync::Arc;
        use tokio::sync::Mutex;

        #[derive(Default)]
        struct Capture {
            authorization: Mutex<Option<String>>,
            body: Mutex<Option<Value>>,
        }

        async fn handler(
            State(capture): State<Arc<Capture>>,
            headers: HeaderMap,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            *capture.authorization.lock().await = headers
                .get(reqwest::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string);
            *capture.body.lock().await = Some(body);
            Json(json!({
                "model": "gpt-4.1",
                "status": "ready",
                "protocol": "openai_chat_completions",
                "invoke": {
                    "url": "https://models.example.com/v1/chat/completions",
                    "timeout_ms": 120000
                }
            }))
        }

        let capture = Arc::new(Capture::default());
        let app = Router::new()
            .route("/resolve", post(handler))
            .with_state(capture.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut gateway = gateway();
        gateway.resolve_url = format!("http://{addr}/resolve");
        let config =
            resolve_model_gateway_invocation(&gateway, &selected_model(), "Bearer runtime-grant")
                .await
                .expect("gateway resolve should succeed");

        assert_eq!(config.url, "https://models.example.com/v1/chat/completions");
        assert_eq!(config.timeout_ms, Some(120_000));
        assert_eq!(
            capture.authorization.lock().await.as_deref(),
            Some("Bearer runtime-grant")
        );
        assert_eq!(
            capture.body.lock().await.as_ref(),
            Some(&json!({
                "model": "gpt-4.1",
                "gateway": "gw-1"
            }))
        );
        server.abort();
    }
}
