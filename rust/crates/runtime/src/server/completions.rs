//! Lightweight `/v1/chat/completions` proxy endpoint.
//!
//! Resolves the active LLM model from the database, forwards the request to
//! the upstream LLM provider, and returns an OpenAI-compatible JSON response.
//! This allows edge components (e.g., LLM judge verification) to make LLM calls
//! through the same authentication and model resolution path as the main agent.

use super::*;
use serde::Deserialize;

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
    let matrixone = crate::matrix_cloud_runtime::matrix_settings_from_env();
    let pool_ref = state.shared_pool.as_ref().map(|sp| sp.get());
    let resolved = astra_services::resolve_active_llm_model(
        &matrixone,
        &state.fernet_encryptor,
        request.model.as_deref(),
        pool_ref,
    )
    .await
    .map_err(|e| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Model resolution failed: {e}"),
        )
    })?;

    // 3. Build upstream request
    let mut body = serde_json::json!({
        "model": resolved.model_name,
        "messages": request.messages,
        "max_tokens": request.max_tokens,
        "temperature": request.temperature,
        "stream": false,
    });

    // Anthropic uses max_tokens; OpenAI prefers max_completion_tokens
    if resolved.provider != "anthropic" && !resolved.model_name.contains("claude") {
        body.as_object_mut().unwrap().remove("max_tokens");
        body["max_completion_tokens"] = serde_json::json!(request.max_tokens);
    }

    let url = format!(
        "{}/chat/completions",
        resolved.base_url.trim_end_matches('/')
    );

    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("HTTP client error: {e}"),
            )
        })?;

    // 4. Forward to upstream LLM provider
    let mut req = client.post(&url).header("content-type", "application/json");
    if resolved.provider == "anthropic" {
        req = req
            .header("x-api-key", &resolved.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "prompt-caching-2024-07-31");
    } else {
        req = req.header("authorization", format!("Bearer {}", resolved.api_key));
    }

    let resp: reqwest::Response = req.json(&body).send().await.map_err(|e| {
        error_response(
            StatusCode::BAD_GATEWAY,
            format!("Upstream LLM error: {e}"),
        )
    })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let truncated = &text[..text.len().min(500)];
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
    let content = upstream["choices"]
        .as_array()
        .and_then(|c| c.first())
        .and_then(|c| c["message"]["content"].as_str())
        .unwrap_or("")
        .to_string();

    let finish_reason = upstream["choices"]
        .as_array()
        .and_then(|c| c.first())
        .and_then(|c| c["finish_reason"].as_str())
        .unwrap_or("stop")
        .to_string();

    let prompt_tokens = upstream["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
    let completion_tokens = upstream["usage"]["completion_tokens"]
        .as_u64()
        .unwrap_or(0);

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
            total_tokens: prompt_tokens + completion_tokens,
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
}
