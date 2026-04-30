//! Lightweight `/v1/chat/completions` proxy endpoint.
//!
//! Resolves the active LLM model from the database, forwards the request to
//! the upstream LLM provider, and returns an OpenAI-compatible JSON response.
//! This allows edge components (e.g., LLM judge verification) to make LLM calls
//! through the same authentication and model resolution path as the main agent.

use super::*;
use serde::Deserialize;
use std::time::Instant;

/// OpenAI-compatible chat completion request (subset).
#[derive(Debug, Deserialize)]
pub(super) struct CompletionRequest {
    /// Optional model name override; falls back to the server's active model.
    #[serde(default)]
    pub model: Option<String>,
    pub messages: Vec<serde_json::Value>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f64,
}

fn default_max_tokens() -> u32 {
    512
}
fn default_temperature() -> f64 {
    0.1
}

/// OpenAI-compatible chat completion response (subset).
#[derive(Debug, Serialize)]
pub(super) struct CompletionResponse {
    pub id: String,
    pub object: String,
    pub model: String,
    pub choices: Vec<CompletionChoice>,
    pub usage: CompletionUsage,
}

#[derive(Debug, Serialize)]
pub(super) struct CompletionChoice {
    pub index: u32,
    pub message: CompletionMessage,
    pub finish_reason: String,
}

#[derive(Debug, Serialize)]
pub(super) struct CompletionMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub(super) struct CompletionUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

/// `POST /v1/chat/completions` — lightweight LLM proxy.
///
/// Authenticates via bearer token, resolves the active LLM model from the database,
/// and forwards a non-streaming chat completion request to the upstream provider.
pub(super) async fn completions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CompletionRequest>,
) -> Result<Json<CompletionResponse>, (StatusCode, Json<ErrorResponse>)> {
    // 1. Authenticate
    let _user = state.auth_service.current_user(&headers).await?;

    // 2. Resolve LLM model
    let matrixone = crate::matrix_cloud_runtime::matrix_settings_from_env().map_err(|e| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("MatrixOne configuration unavailable: {e}"),
        )
    })?;
    let pool_ref = state.shared_pool.as_ref().map(|sp| sp.get());
    // When the caller specifies a model, resolve that exact model (strict).
    // When the caller omits the model (typical for judge / summary proxies), resolve via the
    // admin-config `reasoning_model_name` override, falling back to the cheapest active model.
    let resolved = if let Some(preferred) = request.model.as_deref().filter(|s| !s.is_empty()) {
        astra_services::resolve_active_llm_model(
            &matrixone,
            &state.fernet_encryptor,
            Some(preferred),
            pool_ref,
        )
        .await
    } else {
        astra_services::resolve_reasoning_model(
            &matrixone,
            &state.fernet_encryptor,
            state.admin_config_service.as_ref(),
            pool_ref,
        )
        .await
    }
    .map_err(|e| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Model resolution failed: {e}"),
        )
    })?;

    // 3. Build upstream request
    let mut messages = request.messages;
    crate::turn::llm_client::strip_empty_assistant_tool_calls(&mut messages);
    let body = crate::turn::llm_client::build_provider_request_body(
        &messages,
        &[],
        &resolved.model_name,
        &resolved.provider,
        Some(request.max_tokens as usize),
        Some(request.temperature),
        false,
        &astra_turn_core::thinking_config::ThinkingConfig::Off,
    );

    let url = crate::turn::llm_client::llm_request_url_for_provider(
        &resolved.base_url,
        &resolved.provider,
        &resolved.model_name,
        false,
    );

    let client = &state.http_client;

    // 4. Forward to upstream LLM provider
    let mut req = client.post(&url).header("content-type", "application/json");
    req = crate::turn::llm_client::apply_provider_auth(
        req,
        &resolved.provider,
        &resolved.api_key,
        None,
    );
    if crate::turn::llm_client::provider_uses_anthropic_messages(&resolved.provider) {
        req = req.header("anthropic-beta", "prompt-caching-2024-07-31");
    }

    let resp: reqwest::Response =
        req.json(&body).send().await.map_err(|e| {
            error_response(StatusCode::BAD_GATEWAY, format!("Upstream LLM error: {e}"))
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let truncated = astra_text_utils::str_preview::truncate_str(&text, 500);
        return Err(error_response(
            StatusCode::BAD_GATEWAY,
            format!("Upstream LLM HTTP {status}: {truncated}"),
        ));
    }

    let upstream: serde_json::Value = resp.json().await.map_err(|e| {
        error_response(
            StatusCode::BAD_GATEWAY,
            format!("Failed to parse upstream response: {e}"),
        )
    })?;

    // 5. Extract response and build OpenAI-compatible output
    let parsed = crate::turn::llm_client::parse_nonstream_response_for_provider(
        &upstream,
        &resolved.provider,
        &resolved.model_name,
        Instant::now(),
    );

    let content = parsed.full_text;
    let finish_reason = parsed.finish_reason.unwrap_or_else(|| "stop".to_string());
    let usage = crate::turn::token_usage::TokenUsage::from_json_map(&parsed.usage);
    // OpenAI-compatible response surface: prompt_tokens = fresh + cached + creation,
    // matching upstream `/v1/chat/completions` semantics.
    let prompt_tokens =
        usage.input_tokens + usage.cached_input_tokens + usage.cache_creation_tokens;
    let completion_tokens = usage.output_tokens;

    Ok(Json(CompletionResponse {
        id: upstream["id"]
            .as_str()
            .unwrap_or("chatcmpl-proxy")
            .to_string(),
        object: "chat.completion".to_string(),
        model: resolved.model_name,
        choices: vec![CompletionChoice {
            index: 0,
            message: CompletionMessage {
                role: "assistant".to_string(),
                content,
            },
            finish_reason,
        }],
        usage: CompletionUsage {
            prompt_tokens,
            completion_tokens,
            total_tokens: usage.total_tokens(),
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_request_defaults() {
        let json = r#"{"messages": [{"role": "user", "content": "hello"}]}"#;
        let req: CompletionRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.max_tokens, 512);
        assert!((req.temperature - 0.1).abs() < f64::EPSILON);
        assert!(req.model.is_none());
    }

    #[test]
    fn completion_response_serializes() {
        let resp = CompletionResponse {
            id: "test".into(),
            object: "chat.completion".into(),
            model: "gpt-4o-mini".into(),
            choices: vec![CompletionChoice {
                index: 0,
                message: CompletionMessage {
                    role: "assistant".into(),
                    content: r#"{"score": 0.85}"#.into(),
                },
                finish_reason: "stop".into(),
            }],
            usage: CompletionUsage {
                prompt_tokens: 100,
                completion_tokens: 10,
                total_tokens: 110,
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            json["choices"][0]["message"]["content"],
            r#"{"score": 0.85}"#
        );
    }

    #[test]
    fn completions_handler_strips_empty_assistant_tool_calls_before_forwarding() {
        let mut messages = vec![
            serde_json::json!({"role": "assistant", "content": "Done.", "tool_calls": []}),
            serde_json::json!({"role": "user", "content": "hello"}),
        ];
        crate::turn::llm_client::strip_empty_assistant_tool_calls(&mut messages);
        assert!(messages[0].get("tool_calls").is_none(), "{messages:?}");
    }

    /// audit-C2: completions handler must not use .expect("json object") —
    /// panicking in a request handler crashes the connection.
    #[test]
    fn completions_handler_does_not_expect_json_object() {
        let source = include_str!("completions.rs");
        let test_start = source.find("#[cfg(test)]").unwrap_or(source.len());
        let prod_code = &source[..test_start];
        assert!(
            !prod_code.contains(".expect(\"json object\")"),
            "completions handler must not panic on json object access"
        );
    }
}
