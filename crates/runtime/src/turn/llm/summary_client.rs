use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use serde_json::Value;

use astra_turn_core::cloud_summary::{SummaryLlmClient, SummaryResponse};
use astra_turn_core::thinking_config::ThinkingConfig;
use astra_turn_types::InferencePurpose;

use super::client::{LlmCall, OwnedLlmExecutionRoute, global_llm_client, llm_nonstream_timeout};
use super::durable::DurableInferenceLedger;

#[cfg(test)]
use super::client::call_llm_nonstream;

#[derive(Clone)]
struct DurableSummaryExecution {
    ledger: DurableInferenceLedger,
    base_scope: astra_turn_types::InferenceInvocationScope,
    next_logical_attempt: Arc<AtomicU32>,
}

#[derive(Clone)]
enum SummaryExecution {
    Durable(Box<DurableSummaryExecution>),
    #[cfg(test)]
    Direct,
}

/// Runtime-owned adapter from the provider execution contract to summary work.
/// Provider-specific request construction, authentication, timeouts, and
/// response parsing remain centralized in the canonical LLM client.
#[derive(Clone)]
pub(crate) struct RuntimeSummaryClient {
    route: OwnedLlmExecutionRoute,
    max_output_tokens: usize,
    execution: SummaryExecution,
}

impl RuntimeSummaryClient {
    #[must_use]
    pub fn new(
        route: OwnedLlmExecutionRoute,
        max_output_tokens: usize,
        ledger: DurableInferenceLedger,
        base_scope: astra_turn_types::InferenceInvocationScope,
    ) -> Self {
        Self {
            route,
            max_output_tokens,
            execution: SummaryExecution::Durable(Box::new(DurableSummaryExecution {
                ledger,
                base_scope,
                next_logical_attempt: Arc::new(AtomicU32::new(0)),
            })),
        }
    }

    /// Low-level provider-adapter constructor for unit tests. Production
    /// summary paths must use [`Self::new`] so auxiliary calls cannot bypass
    /// durable admission and usage settlement.
    #[cfg(test)]
    #[must_use]
    pub fn new_direct_for_test(route: OwnedLlmExecutionRoute, max_output_tokens: usize) -> Self {
        Self {
            route,
            max_output_tokens,
            execution: SummaryExecution::Direct,
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
        let call = LlmCall {
            purpose,
            messages,
            tools: &[],
            route: self.route.borrowed(),
            max_output_tokens: Some(self.max_output_tokens),
            temperature: None,
            has_fallback: false,
            thinking: &ThinkingConfig::Off,
        };
        let result = match &self.execution {
            SummaryExecution::Durable(execution) => {
                let DurableSummaryExecution {
                    ledger,
                    base_scope,
                    next_logical_attempt,
                } = execution.as_ref();
                let logical_attempt = next_logical_attempt.fetch_add(1, Ordering::AcqRel);
                ledger
                    .execute_nonstream(
                        global_llm_client(),
                        base_scope.with_logical_attempt(logical_attempt),
                        call,
                        llm_nonstream_timeout(),
                    )
                    .await
            }
            #[cfg(test)]
            SummaryExecution::Direct => {
                call_llm_nonstream(global_llm_client(), call, llm_nonstream_timeout()).await
            }
        };
        match result {
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
