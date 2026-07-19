use async_trait::async_trait;
use serde_json::Value;

use astra_turn_core::cloud_summary::{SummaryLlmClient, SummaryResponse};
use astra_turn_core::thinking_config::ThinkingConfig;
use astra_turn_types::InferencePurpose;

use super::client::{
    LlmCall, OwnedLlmExecutionRoute, call_llm_nonstream, global_llm_client, llm_nonstream_timeout,
};

/// Runtime-owned adapter from the provider execution contract to summary work.
/// Provider-specific request construction, authentication, timeouts, and
/// response parsing remain centralized in the canonical LLM client.
#[derive(Debug, Clone)]
pub(crate) struct RuntimeSummaryClient {
    route: OwnedLlmExecutionRoute,
    max_output_tokens: usize,
}

impl RuntimeSummaryClient {
    #[must_use]
    pub fn new(route: OwnedLlmExecutionRoute, max_output_tokens: usize) -> Self {
        Self {
            route,
            max_output_tokens,
        }
    }
}

#[async_trait]
impl SummaryLlmClient for RuntimeSummaryClient {
    async fn summarize(
        &self,
        purpose: InferencePurpose,
        messages: &[Value],
    ) -> Result<SummaryResponse, String> {
        match call_llm_nonstream(
            global_llm_client(),
            LlmCall {
                purpose,
                messages,
                tools: &[],
                route: self.route.borrowed(),
                max_output_tokens: Some(self.max_output_tokens),
                temperature: None,
                has_fallback: false,
                thinking: &ThinkingConfig::Off,
            },
            llm_nonstream_timeout(),
        )
        .await
        {
            Ok(result) => Ok(SummaryResponse {
                text: result.full_text,
                is_ptl_error: false,
            }),
            Err(error) if error.kind == astra_core::ErrorKind::ContextWindow => {
                Ok(SummaryResponse {
                    text: String::new(),
                    is_ptl_error: true,
                })
            }
            Err(error) => Err(error.to_string()),
        }
    }
}
