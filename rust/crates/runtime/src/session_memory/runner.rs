//! Pure async worker body for one extraction attempt.
//!
//! Writes always land (when a Memoria client is available): if the LLM
//! fails (timeout, transport, empty), the runner falls through to
//! [`astra_turn_core::cloud_session_memory_extract::build_l1_from_messages`]
//! so the L1 at least reflects the conversation head.
//!
//! The worker **does not emit events**. Eventing is the service's job
//! (it owns user_id + ingestion sender + broker). The worker just
//! reports which artifact it wrote.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use astra_services::session_journal::{
    SessionMemoryExtractionErrorReason, SessionMemoryExtractionSource,
};
use astra_turn_core::cloud_session_memory_extract::build_extraction_prompt;
use astra_turn_core::cloud_summary::{
    HttpSummaryClient, LlmConnParams as SummaryConnParams, SummaryLlmClient,
};

use crate::memory_relevance::LlmConnParams;
use crate::turn::cloud::memoria_compact::MemoriaClient;
use crate::turn::cloud::session_memory_protocol::{PersistL1Error, persist_l1};

/// What the worker produced. Service inspects this to decide which
/// journal event + broker signal to emit.
pub enum ExtractionArtifacts {
    /// LLM or rule-based wrote successfully to Memoria.
    Persisted {
        source: SessionMemoryExtractionSource,
        bytes_written: u64,
        /// How many store attempts were needed (1 or 2). Surfaced as
        /// an event breadcrumb so operators can see retry incidence
        /// without grepping logs.
        store_attempt: u32,
    },
    /// LLM attempted and failed; rule-based fallback was persisted
    /// successfully. Service records both the error and the write.
    LlmFailedPersistedFallback {
        error_reason: SessionMemoryExtractionErrorReason,
        bytes_written: u64,
        store_attempt: u32,
    },
    /// Memoria persist failed. Nothing landed. Reason is one of
    /// `PurgeFailed` / `WriteFailed`.
    PersistFailed {
        error_reason: SessionMemoryExtractionErrorReason,
    },
}

/// Run one extraction.
///
/// `selector_params: Some` → attempt LLM, fall back on failure;
/// `None` → rule-based only, no LLM call.
#[allow(clippy::too_many_arguments)]
pub async fn run_extraction(
    memoria: &Arc<dyn MemoriaClient>,
    session_id: &str,
    messages: &[Value],
    turn_number: usize,
    current_tokens: usize,
    current_memory: &str,
    selector_params: Option<&LlmConnParams>,
    llm_timeout: Duration,
    max_output_tokens: usize,
) -> ExtractionArtifacts {
    // LLM attempt (when configured). On failure fall through to
    // rule-based but keep the error reason for the caller to log.
    let (content, source, llm_error) = match selector_params {
        Some(params) => {
            let (maybe_text, err) = try_llm_extraction(
                params,
                current_memory,
                messages,
                llm_timeout,
                max_output_tokens,
            )
            .await;
            match (maybe_text, err) {
                (Some(text), _) => (text, SessionMemoryExtractionSource::Llm, None),
                (None, Some(e)) => (
                    rule_based(messages, turn_number, current_tokens),
                    SessionMemoryExtractionSource::RuleFallback,
                    Some(e),
                ),
                (None, None) => (
                    rule_based(messages, turn_number, current_tokens),
                    SessionMemoryExtractionSource::RuleFallback,
                    Some(SessionMemoryExtractionErrorReason::EmptyResponse),
                ),
            }
        }
        None => (
            rule_based(messages, turn_number, current_tokens),
            SessionMemoryExtractionSource::RuleFallback,
            None,
        ),
    };

    let persist = match persist_l1(memoria.as_ref(), &content, session_id).await {
        Ok(success) => success,
        Err(e) => {
            tracing::debug!(
                session_id = %session_id,
                error = %e,
                "session-memory extraction: Memoria persist failed"
            );
            let error_reason = match e {
                PersistL1Error::PurgeFailed(_) => SessionMemoryExtractionErrorReason::PurgeFailed,
                PersistL1Error::StoreFailed(_) => SessionMemoryExtractionErrorReason::WriteFailed,
            };
            return ExtractionArtifacts::PersistFailed { error_reason };
        }
    };

    let bytes_written = content.len() as u64;
    match llm_error {
        Some(reason) => ExtractionArtifacts::LlmFailedPersistedFallback {
            error_reason: reason,
            bytes_written,
            store_attempt: persist.store_attempt,
        },
        None => ExtractionArtifacts::Persisted {
            source,
            bytes_written,
            store_attempt: persist.store_attempt,
        },
    }
}

fn rule_based(messages: &[Value], turn_number: usize, current_tokens: usize) -> String {
    crate::turn::cloud::session_memory_protocol::build_l1_from_messages(
        messages,
        turn_number,
        current_tokens,
    )
}

async fn try_llm_extraction(
    params: &LlmConnParams,
    current_memory: &str,
    messages: &[Value],
    timeout: Duration,
    max_output_tokens: usize,
) -> (Option<String>, Option<SessionMemoryExtractionErrorReason>) {
    let client = HttpSummaryClient::new(SummaryConnParams {
        model_name: params.model_name.clone(),
        api_key: params.api_key.clone(),
        base_url: params.base_url.clone(),
        provider: params.provider.clone(),
        max_output_tokens,
    });
    let prompt = build_extraction_prompt(current_memory, messages);
    match tokio::time::timeout(timeout, client.summarize(&prompt)).await {
        Ok(Ok(resp)) if !resp.is_ptl_error && !resp.text.trim().is_empty() => {
            (Some(resp.text), None)
        }
        Ok(Ok(_)) => (
            None,
            Some(SessionMemoryExtractionErrorReason::EmptyResponse),
        ),
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "session-memory extraction: LLM error");
            (None, Some(SessionMemoryExtractionErrorReason::LlmError))
        }
        Err(_) => {
            tracing::debug!(
                "session-memory extraction: LLM call exceeded {}s timeout",
                timeout.as_secs()
            );
            (None, Some(SessionMemoryExtractionErrorReason::LlmTimeout))
        }
    }
}
