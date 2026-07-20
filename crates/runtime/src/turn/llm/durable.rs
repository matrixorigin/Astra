//! Runtime boundary between admitted model execution and the durable inference ledger.
//!
//! Callers own semantic purpose and causal scope. This module owns the invariant
//! that route/invocation admission happens before provider I/O, every physical
//! request is observed, and the logical terminal matches the provider terminal.

use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use serde_json::json;

use crate::turn::llm::client::{LlmCall, LlmCallResult, ProviderAttemptObserver};
use astra_core::SharedPool;

const INFERENCE_LEDGER_ERROR_SOURCE: &str = "inference_execution_ledger";

#[derive(Clone)]
pub(crate) struct DurableInferenceLedger {
    shared_pool: SharedPool,
    user_id: String,
    admitted_execution: astra_services::AdmittedModelExecution,
}

impl DurableInferenceLedger {
    pub(crate) fn new(
        shared_pool: SharedPool,
        user_id: impl Into<String>,
        admitted_execution: astra_services::AdmittedModelExecution,
    ) -> Self {
        Self {
            shared_pool,
            user_id: user_id.into(),
            admitted_execution,
        }
    }

    pub(crate) fn from_optional(
        shared_pool: Option<&SharedPool>,
        admitted_execution: Option<&astra_services::AdmittedModelExecution>,
        user_id: &str,
    ) -> Result<Option<Self>, astra_core::ClassifiedError> {
        let Some(shared_pool) = shared_pool else {
            return Ok(None);
        };
        let admitted_execution = admitted_execution.ok_or_else(|| {
            contract_error(
                "admission",
                "Server execution has no admitted Offering material",
            )
        })?;
        Ok(Some(Self::new(
            shared_pool.clone(),
            user_id,
            admitted_execution.clone(),
        )))
    }

    pub(crate) async fn admit(
        &self,
        scope: astra_turn_types::InferenceInvocationScope,
        purpose: astra_turn_types::InferencePurpose,
        resolved_model_name: &str,
        upstream_model_name: &str,
        provider: &str,
    ) -> Result<DurableInferenceInvocation, astra_core::ClassifiedError> {
        if self.admitted_execution.model_name != resolved_model_name
            || self.admitted_execution.provider != provider
            || self
                .admitted_execution
                .wire_model_name
                .as_deref()
                .unwrap_or(&self.admitted_execution.model_name)
                != upstream_model_name
        {
            return Err(contract_error(
                "admission",
                "resolved provider route drifted from the admitted Offering",
            ));
        }
        let plan =
            astra_services::plan_inference_invocation(astra_services::InferenceInvocationInput {
                user_id: self.user_id.clone(),
                scope,
                offering_id: self.admitted_execution.offering_id.clone(),
                resolved_model_name: resolved_model_name.to_string(),
                upstream_model_name: upstream_model_name.to_string(),
                provider: provider.to_string(),
                purpose,
                execution_placement: self.admitted_execution.execution_placement,
                access_kind: self.admitted_execution.access_kind,
            })
            .map_err(|error| service_error("planning", error))?;
        astra_services::admit_inference_invocation(&self.shared_pool, &plan)
            .await
            .map_err(|error| service_error("admission", error))?;
        Ok(DurableInferenceInvocation {
            observer: DurableProviderAttemptObserver::new(self.shared_pool.clone(), plan.clone()),
            shared_pool: self.shared_pool.clone(),
            plan,
        })
    }

    pub(crate) async fn execute_nonstream(
        &self,
        client: &reqwest::Client,
        scope: astra_turn_types::InferenceInvocationScope,
        call: LlmCall<'_>,
        timeout: std::time::Duration,
    ) -> Result<LlmCallResult, astra_core::ClassifiedError> {
        let invocation = self
            .admit(
                scope,
                call.purpose,
                call.route.model_name,
                call.route.wire_model_name.unwrap_or(call.route.model_name),
                call.route.provider,
            )
            .await?;
        let result = crate::turn::llm::client::call_llm_nonstream_with_attempt_observer(
            client,
            call,
            timeout,
            Some(invocation.attempt_observer()),
        )
        .await;
        match result {
            Ok(result) => {
                invocation.finish_result(&result).await?;
                Ok(result)
            }
            Err(error) => {
                invocation.finish_error(&error).await?;
                Err(error)
            }
        }
    }
}

pub(crate) struct DurableInferenceInvocation {
    shared_pool: SharedPool,
    plan: astra_services::InferenceInvocationPlan,
    observer: DurableProviderAttemptObserver,
}

impl DurableInferenceInvocation {
    pub(crate) fn attempt_observer(&self) -> &dyn ProviderAttemptObserver {
        &self.observer
    }

    pub(crate) async fn finish_result(
        &self,
        result: &LlmCallResult,
    ) -> Result<(), astra_core::ClassifiedError> {
        let usage = crate::turn::token_usage::TokenUsage::from_partial_json_map(&result.usage);
        self.finish(&astra_services::InferenceInvocationTerminal::succeeded(
            astra_services::InferenceUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: usage.cached_input_tokens,
                cache_creation_tokens: usage.cache_creation_tokens,
            },
            result.response_id.clone(),
        ))
        .await
    }

    pub(crate) async fn finish_error(
        &self,
        error: &astra_core::ClassifiedError,
    ) -> Result<(), astra_core::ClassifiedError> {
        // An observer/ledger failure means provider delivery or the durable
        // provider terminal may be unknown. Keep the logical invocation in
        // `admitted` for reconciliation instead of inventing an outcome.
        if is_ledger_error(error) {
            return Ok(());
        }
        self.finish(&terminal_from_error(error)).await
    }

    pub(crate) async fn finish(
        &self,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError> {
        astra_services::finish_inference_invocation(&self.shared_pool, &self.plan, terminal)
            .await
            .map_err(|error| service_error("terminal commit", error))
    }
}

struct DurableProviderAttemptObserver {
    shared_pool: SharedPool,
    invocation: astra_services::InferenceInvocationPlan,
    next_attempt: AtomicU32,
}

impl DurableProviderAttemptObserver {
    fn new(shared_pool: SharedPool, invocation: astra_services::InferenceInvocationPlan) -> Self {
        Self {
            shared_pool,
            invocation,
            next_attempt: AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl ProviderAttemptObserver for DurableProviderAttemptObserver {
    async fn begin_attempt(&self) -> Result<u32, astra_core::ClassifiedError> {
        let attempt_index = self.next_attempt.fetch_add(1, Ordering::AcqRel);
        let attempt =
            astra_services::plan_inference_provider_attempt(&self.invocation, attempt_index);
        astra_services::begin_inference_provider_attempt(&self.shared_pool, &attempt)
            .await
            .map_err(|error| service_error("provider attempt admission", error))?;
        Ok(attempt_index)
    }

    async fn finish_attempt(
        &self,
        attempt_index: u32,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError> {
        let attempt =
            astra_services::plan_inference_provider_attempt(&self.invocation, attempt_index);
        astra_services::finish_inference_provider_attempt(&self.shared_pool, &attempt, terminal)
            .await
            .map_err(|error| service_error("provider attempt terminal commit", error))
    }
}

fn contract_error(
    stage: &'static str,
    error: impl std::fmt::Display,
) -> astra_core::ClassifiedError {
    astra_core::ClassifiedError::new(
        astra_core::ErrorKind::ContractViolation,
        format!("durable inference {stage} failed: {error}"),
    )
    .with_details_json(
        json!({
            "source": INFERENCE_LEDGER_ERROR_SOURCE,
            "stage": stage,
        })
        .to_string(),
    )
}

fn service_error(
    stage: &'static str,
    error: astra_services::ServiceError,
) -> astra_core::ClassifiedError {
    let kind = match error.kind {
        astra_services::ServiceErrorKind::Persistence => astra_core::ErrorKind::DatabaseError,
        astra_services::ServiceErrorKind::Network => astra_core::ErrorKind::Network,
        astra_services::ServiceErrorKind::Invalid | astra_services::ServiceErrorKind::NotFound => {
            astra_core::ErrorKind::InvalidRequest
        }
        astra_services::ServiceErrorKind::Verification
        | astra_services::ServiceErrorKind::Conflict
        | astra_services::ServiceErrorKind::ConflictTransient
        | astra_services::ServiceErrorKind::Internal => astra_core::ErrorKind::ContractViolation,
    };
    astra_core::ClassifiedError::new(kind, format!("durable inference {stage} failed: {error}"))
        .with_details_json(
            json!({
                "source": INFERENCE_LEDGER_ERROR_SOURCE,
                "stage": stage,
                "service_error_kind": error.kind.as_str(),
            })
            .to_string(),
        )
}

pub(crate) fn is_ledger_error(error: &astra_core::ClassifiedError) -> bool {
    error
        .details_json
        .as_deref()
        .and_then(|details| serde_json::from_str::<serde_json::Value>(details).ok())
        .and_then(|details| {
            details
                .get("source")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|source| source == INFERENCE_LEDGER_ERROR_SOURCE)
}

pub(crate) fn terminal_from_error(
    error: &astra_core::ClassifiedError,
) -> astra_services::InferenceInvocationTerminal {
    let status = match error.kind {
        astra_core::ErrorKind::Cancelled => astra_services::InferenceTerminalStatus::Cancelled,
        astra_core::ErrorKind::Network
        | astra_core::ErrorKind::StreamIdle
        | astra_core::ErrorKind::StreamTransport => {
            astra_services::InferenceTerminalStatus::DeliveryUnknown
        }
        _ => astra_services::InferenceTerminalStatus::Failed,
    };
    let message = crate::turn::llm::client::redact_provider_secrets(&error.message);
    astra_services::InferenceInvocationTerminal {
        status,
        usage: astra_services::InferenceUsage::default(),
        provider_response_id: None,
        error_kind: Some(error.kind.as_str().to_string()),
        error_message: Some(
            astra_text_utils::str_preview::truncate_str(&message, 1_000).to_string(),
        ),
    }
}
