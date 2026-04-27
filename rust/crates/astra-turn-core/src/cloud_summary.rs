//! LLM-based conversation summarization for compaction.
//!
//! This module provides [`generate_compact_summary`] which calls the LLM to
//! produce a dense semantic summary of conversation history. Used by Phase 2
//! compaction when tier >= AggressivePrune and LLM summary is enabled.
//!
//! Design principles:
//! - **PTL retry**: if the summary request itself exceeds the context window,
//!   drop the oldest API rounds and retry (up to [`MAX_PTL_RETRIES`]).
//! - **Fallback**: if retries are exhausted, return `None` so callers can
//!   fall back to pure truncation.
//! - **Testable**: the LLM call is abstracted behind [`SummaryLlmClient`] so
//!   tests can inject mock responses without a real API.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{
    cloud_compact_prompt::{
        COMPACT_SYSTEM_PROMPT, build_compact_user_prompt, render_messages_for_summary,
    },
    cloud_grouping::{drop_oldest_rounds, flatten_rounds, group_by_api_round},
};

/// Maximum number of PTL retry attempts before giving up and returning `None`.
pub const MAX_PTL_RETRIES: usize = 3;

/// Minimum number of API rounds to keep when dropping for PTL retry.
pub const MIN_ROUNDS_TO_KEEP: usize = 1;

// ---------------------------------------------------------------------------
// LLM client abstraction (for testability)
// ---------------------------------------------------------------------------

/// Result of a single LLM summary call.
#[derive(Debug, Clone)]
pub struct SummaryResponse {
    /// The generated summary text.
    pub text: String,
    /// Whether the request exceeded the context window (PTL error).
    pub is_ptl_error: bool,
}

/// Connection parameters for the LLM API.
#[derive(Clone)]
pub struct LlmConnParams {
    pub model_name: String,
    pub api_key: String,
    pub base_url: String,
    pub provider: String,
    /// Maximum tokens to generate for the summary.
    pub max_output_tokens: usize,
}

impl std::fmt::Debug for LlmConnParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmConnParams")
            .field("model_name", &self.model_name)
            .field("api_key", &"[REDACTED]")
            .field("base_url", &self.base_url)
            .field("provider", &self.provider)
            .field("max_output_tokens", &self.max_output_tokens)
            .finish()
    }
}

impl LlmConnParams {
    /// Build from environment variables.
    ///
    /// Required: `MO_MODEL`, `MO_API_KEY`, `MO_BASE_URL`
    /// Optional: `MO_LLM_PROVIDER` (default: "openai"), `MO_MAX_OUTPUT_TOKENS` (default: 4096)
    pub fn from_env() -> Option<Self> {
        let model_name = std::env::var("MO_MODEL").ok()?;
        let api_key = std::env::var("MO_API_KEY").ok()?;
        let base_url = std::env::var("MO_BASE_URL").ok()?;
        if model_name.is_empty() || api_key.is_empty() || base_url.is_empty() {
            return None;
        }
        let provider = std::env::var("MO_LLM_PROVIDER").unwrap_or_else(|_| "openai".to_string());
        let max_output_tokens = std::env::var("MO_MAX_OUTPUT_TOKENS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(4096);
        Some(Self {
            model_name,
            api_key,
            base_url,
            provider,
            max_output_tokens,
        })
    }
}

/// Abstraction over the LLM API for summary generation.
/// Implemented by the real HTTP client and by mocks in tests.
#[async_trait]
pub trait SummaryLlmClient: Send + Sync {
    /// Send a summary request. Returns the response or an error.
    async fn summarize(&self, messages: &[Value]) -> Result<SummaryResponse, String>;
}

// ---------------------------------------------------------------------------
// Core summary generation
// ---------------------------------------------------------------------------

/// Generate a compact summary for `messages` using the provided LLM client.
///
/// Returns `Some(summary_text)` on success, or `None` if all retries are
/// exhausted (callers should fall back to truncation).
///
/// PTL retry behaviour:
/// 1. Render messages into compaction prompt
/// 2. Call LLM
/// 3. If PTL error: drop oldest round and retry (up to `MAX_PTL_RETRIES`)
/// 4. If other error: return `None` immediately
pub async fn generate_compact_summary(
    messages: &[Value],
    client: &dyn SummaryLlmClient,
) -> Option<String> {
    let (system_msgs, mut rounds) = group_by_api_round(messages);
    let min_keep = MIN_ROUNDS_TO_KEEP;

    for attempt in 0..=MAX_PTL_RETRIES {
        let msgs_for_summary = flatten_rounds(&system_msgs, &rounds);
        let rendered = render_messages_for_summary(&msgs_for_summary);
        let prompt_messages = build_summary_messages(&rendered);

        match client.summarize(&prompt_messages).await {
            Ok(resp) if !resp.is_ptl_error => {
                return Some(crate::cloud_compact_prompt::format_structured_summary(
                    &resp.text,
                ));
            }
            Ok(resp) if resp.is_ptl_error => {
                if attempt >= MAX_PTL_RETRIES {
                    eprintln!(
                        "[compact_summary] PTL retries exhausted after {} attempts, falling back to truncation",
                        attempt
                    );
                    return None;
                }
                // Drop the oldest round and retry
                let rounds_before = rounds.len();
                let new_rounds = drop_oldest_rounds(&rounds, 1, min_keep);
                if new_rounds.len() == rounds_before {
                    // Can't drop any more rounds
                    eprintln!("[compact_summary] cannot drop more rounds, giving up");
                    return None;
                }
                eprintln!(
                    "[compact_summary] PTL error, dropping oldest round (attempt {}, {} → {} rounds)",
                    attempt,
                    rounds_before,
                    new_rounds.len()
                );
                rounds = new_rounds.to_vec();
            }
            Ok(_) => unreachable!(),
            Err(e) => {
                eprintln!("[compact_summary] LLM error: {e}, falling back to truncation");
                return None;
            }
        }
    }

    None
}

/// Build the messages array for the summary LLM call.
fn build_summary_messages(rendered_conversation: &str) -> Vec<Value> {
    vec![
        serde_json::json!({
            "role": "system",
            "content": COMPACT_SYSTEM_PROMPT,
        }),
        serde_json::json!({
            "role": "user",
            "content": build_compact_user_prompt(rendered_conversation),
        }),
    ]
}

// ---------------------------------------------------------------------------
// HTTP implementation
// ---------------------------------------------------------------------------

/// Real HTTP-based summary client using the runtime's LLM gateway.
pub struct HttpSummaryClient {
    params: LlmConnParams,
}

impl HttpSummaryClient {
    pub fn new(params: LlmConnParams) -> Self {
        Self { params }
    }
}

#[async_trait]
impl SummaryLlmClient for HttpSummaryClient {
    async fn summarize(&self, messages: &[Value]) -> Result<SummaryResponse, String> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| e.to_string())?;

        let body = if self.params.provider == "bedrock" {
            let system = messages
                .iter()
                .filter(|msg| msg.get("role").and_then(Value::as_str) == Some("system"))
                .filter_map(|msg| msg.get("content").and_then(Value::as_str))
                .map(|text| json!({ "text": text }))
                .collect::<Vec<_>>();
            let bedrock_messages = messages
                .iter()
                .filter(|msg| msg.get("role").and_then(Value::as_str) != Some("system"))
                .filter_map(|msg| {
                    let role = msg.get("role").and_then(Value::as_str)?;
                    let content = msg.get("content").and_then(Value::as_str)?;
                    Some(json!({
                        "role": if role == "assistant" { "assistant" } else { "user" },
                        "content": [{ "text": content }],
                    }))
                })
                .collect::<Vec<_>>();
            let mut body = json!({
                "messages": bedrock_messages,
                "inferenceConfig": {
                    "maxTokens": self.params.max_output_tokens,
                }
            });
            if !system.is_empty() {
                body["system"] = Value::Array(system);
            }
            body
        } else if self.params.provider == "anthropic" {
            json!({
                "model": self.params.model_name,
                "messages": messages,
                "stream": false,
                "max_tokens": self.params.max_output_tokens,
            })
        } else {
            json!({
                "model": self.params.model_name,
                "messages": messages,
                "stream": false,
                "max_completion_tokens": self.params.max_output_tokens,
            })
        };

        let url = if self.params.provider == "bedrock" {
            let mut url = reqwest::Url::parse(self.params.base_url.trim_end_matches('/'))
                .map_err(|e| e.to_string())?;
            {
                let mut segments = url
                    .path_segments_mut()
                    .map_err(|_| "invalid Bedrock base_url")?;
                segments.pop_if_empty();
                segments.push("model");
                segments.push(&self.params.model_name);
                segments.push("converse");
            }
            url.to_string()
        } else if self.params.provider == "anthropic" {
            let base = self.params.base_url.trim_end_matches('/');
            if base.ends_with("/v1") {
                format!("{base}/messages")
            } else {
                format!("{base}/v1/messages")
            }
        } else {
            format!(
                "{}/chat/completions",
                self.params.base_url.trim_end_matches('/')
            )
        };

        let mut req = client.post(&url).header("content-type", "application/json");
        if self.params.provider == "anthropic" {
            req = req
                .header("x-api-key", &self.params.api_key)
                .header("anthropic-version", "2023-06-01");
        } else {
            req = req.header("authorization", format!("Bearer {}", self.params.api_key));
        }

        let resp = req.json(&body).send().await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();

        if status == 400 {
            let body_text = resp.text().await.unwrap_or_default();
            if is_ptl_error(&body_text) {
                return Ok(SummaryResponse {
                    text: String::new(),
                    is_ptl_error: true,
                });
            }
            return Err(format!("LLM 400 error: {body_text}"));
        }

        if !resp.status().is_success() {
            return Err(format!("LLM error status: {status}"));
        }

        let json: Value = resp.json().await.map_err(|e| e.to_string())?;
        let text = if self.params.provider == "bedrock" {
            json.get("output")
                .and_then(|output| output.get("message"))
                .and_then(|message| message.get("content"))
                .and_then(Value::as_array)
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|block| block.get("text").and_then(Value::as_str))
                        .collect::<String>()
                })
                .unwrap_or_default()
        } else {
            json.get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string()
        };

        Ok(SummaryResponse {
            text,
            is_ptl_error: false,
        })
    }
}

/// Detect if an error response body indicates a context-too-long error.
fn is_ptl_error(body: &str) -> bool {
    let lower = body.to_lowercase();
    lower.contains("context_length_exceeded")
        || lower.contains("maximum context length")
        || lower.contains("prompt is too long")
        || lower.contains("too many tokens")
        || lower.contains("input is too long")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test helpers exposed for cross-crate testing (e.g. runtime's compaction tests).
pub mod test_support {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock LLM client for testing.
    pub struct MockSummaryClient {
        /// Responses to return in order. If fewer than calls, last is repeated.
        pub responses: Vec<Result<SummaryResponse, String>>,
        pub call_count: Arc<AtomicUsize>,
    }

    impl MockSummaryClient {
        pub fn success(text: &str) -> Self {
            Self {
                responses: vec![Ok(SummaryResponse {
                    text: text.to_string(),
                    is_ptl_error: false,
                })],
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        pub fn ptl_then_success(success_text: &str) -> Self {
            Self {
                responses: vec![
                    Ok(SummaryResponse {
                        text: String::new(),
                        is_ptl_error: true,
                    }),
                    Ok(SummaryResponse {
                        text: success_text.to_string(),
                        is_ptl_error: false,
                    }),
                ],
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        pub fn always_ptl() -> Self {
            Self {
                responses: vec![Ok(SummaryResponse {
                    text: String::new(),
                    is_ptl_error: true,
                })],
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        pub fn error(msg: &str) -> Self {
            Self {
                responses: vec![Err(msg.to_string())],
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait]
    impl SummaryLlmClient for MockSummaryClient {
        async fn summarize(&self, _messages: &[Value]) -> Result<SummaryResponse, String> {
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);
            let idx = count.min(self.responses.len() - 1);
            self.responses[idx].clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::MockSummaryClient;
    use super::*;
    use serde_json::json;
    use std::sync::atomic::Ordering;

    fn make_messages(n: usize) -> Vec<Value> {
        (0..n)
            .flat_map(|i| {
                vec![
                    json!({"role": "user", "content": format!("question {i}")}),
                    json!({"role": "assistant", "content": format!("answer {i}")}),
                ]
            })
            .collect()
    }

    #[tokio::test]
    async fn success_on_first_attempt() {
        let body = "### Primary Request\nDoing stuff\n### Pending Tasks\nNone\n### Current Work\nIn progress\n### Current State\nDone";
        let client = MockSummaryClient::success(body);
        let msgs = make_messages(3);
        let result = generate_compact_summary(&msgs, &client).await;
        assert_eq!(result.as_deref(), Some(body));
        assert_eq!(client.call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn ptl_retry_drops_oldest_round_and_succeeds() {
        let body = "### Primary Request\nX\n### Pending Tasks\nY\n### Current Work\nW\n### Current State\nZ";
        let client = MockSummaryClient::ptl_then_success(body);
        let msgs = make_messages(4); // 4 rounds, enough to drop one
        let result = generate_compact_summary(&msgs, &client).await;
        assert_eq!(result.as_deref(), Some(body));
        assert_eq!(client.call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn returns_none_when_all_retries_exhausted() {
        let client = MockSummaryClient::always_ptl();
        // Only 1 round — can't drop any, gives up
        let msgs = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "hello"}),
        ];
        let result = generate_compact_summary(&msgs, &client).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn returns_none_on_llm_error() {
        let client = MockSummaryClient::error("connection refused");
        let msgs = make_messages(2);
        let result = generate_compact_summary(&msgs, &client).await;
        assert!(result.is_none());
    }

    #[test]
    fn is_ptl_error_detects_known_patterns() {
        assert!(is_ptl_error("context_length_exceeded"));
        assert!(is_ptl_error("maximum context length is 128000"));
        assert!(is_ptl_error("Prompt is too long for this model"));
        assert!(is_ptl_error("too many tokens in input"));
        assert!(!is_ptl_error("some other error"));
        assert!(!is_ptl_error(""));
    }

    #[test]
    fn build_summary_messages_structure() {
        let msgs = build_summary_messages("some conversation");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"].as_str().unwrap(), "system");
        assert_eq!(msgs[1]["role"].as_str().unwrap(), "user");
        assert!(
            msgs[1]["content"]
                .as_str()
                .unwrap()
                .contains("some conversation")
        );
    }

    #[tokio::test]
    async fn ptl_retry_with_minimum_rounds() {
        // Exactly 2 messages (1 round) — can't drop below minimum, returns None
        let client = MockSummaryClient::always_ptl();
        let msgs = vec![
            json!({"role": "user", "content": "single question"}),
            json!({"role": "assistant", "content": "single answer"}),
        ];
        let result = generate_compact_summary(&msgs, &client).await;
        assert!(result.is_none());
        // Should give up quickly — can't drop the only round
        assert!(client.call_count.load(Ordering::SeqCst) <= 2);
    }
}
