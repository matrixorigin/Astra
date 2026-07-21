//! Runtime boundary between admitted model execution and the durable inference ledger.
//!
//! Callers own semantic purpose and causal scope. This module owns the invariant
//! that route/invocation admission happens before provider I/O, every physical
//! request is observed, and the logical terminal matches the provider terminal.

use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

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
            observer: Arc::new(DurableProviderAttemptObserver::new(
                self.shared_pool.clone(),
                plan.clone(),
            )),
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
        let attempt_observer = invocation.attempt_observer_arc();
        let settlement = NonstreamInvocationSupervisor::start(Arc::new(invocation));
        let result = crate::turn::llm::client::call_llm_nonstream_with_attempt_observer(
            client,
            call,
            timeout,
            Some(attempt_observer.as_ref()),
        )
        .await;
        match result {
            Ok(result) => {
                if let Err(e) = settlement
                    .settle(NonstreamSettlementCommand::Terminal(terminal_from_result(
                        &result,
                    )))
                    .await
                {
                    tracing::error!(
                        ?result.response_id,
                        %e,
                        "LLM call succeeded and its provider attempt terminal was recorded, but logical invocation settlement failed"
                    );
                    return Err(e);
                }
                Ok(result)
            }
            Err(error) => {
                let command = if is_ledger_error(&error) {
                    // Provider delivery or its durable terminal is unknown.
                    // Preserve the admitted row for reconciliation instead of
                    // inventing a logical outcome.
                    NonstreamSettlementCommand::LeaveAdmitted
                } else {
                    NonstreamSettlementCommand::Terminal(terminal_from_error(&error))
                };
                if let Err(e) = settlement.settle(command).await {
                    tracing::error!(
                        %error,
                        %e,
                        "LLM call failed and its provider attempt terminal was recorded, but logical invocation settlement failed"
                    );
                    return Err(e);
                }
                Err(error)
            }
        }
    }
}

#[async_trait]
trait NonstreamInvocationSettlement: Send + Sync + 'static {
    async fn settle_terminal(
        &self,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError>;

    async fn settle_caller_drop(&self) -> Result<(), astra_core::ClassifiedError>;
}

enum NonstreamSettlementCommand {
    Terminal(astra_services::InferenceInvocationTerminal),
    LeaveAdmitted,
}

/// Owns logical settlement independently of the caller future.
///
/// Dropping the caller closes `command_tx`, but Tokio keeps the detached task
/// alive so it can converge the durable attempt and invocation. Once a normal
/// provider outcome is sent, the same task remains the sole terminal writer
/// even if the caller is cancelled while awaiting the durable commit.
struct NonstreamInvocationSupervisor {
    command_tx: Option<tokio::sync::oneshot::Sender<NonstreamSettlementCommand>>,
    task: tokio::task::JoinHandle<Result<(), astra_core::ClassifiedError>>,
}

impl NonstreamInvocationSupervisor {
    fn start(owner: Arc<dyn NonstreamInvocationSettlement>) -> Self {
        let (command_tx, command_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            match command_rx.await {
                Ok(NonstreamSettlementCommand::Terminal(terminal)) => {
                    owner.settle_terminal(&terminal).await
                }
                Ok(NonstreamSettlementCommand::LeaveAdmitted) => Ok(()),
                Err(_) => {
                    let result = owner.settle_caller_drop().await;
                    if let Err(error) = &result {
                        tracing::error!(
                            %error,
                            "detached non-streaming inference settlement failed after caller cancellation"
                        );
                    }
                    result
                }
            }
        });
        Self {
            command_tx: Some(command_tx),
            task,
        }
    }

    async fn settle(
        mut self,
        command: NonstreamSettlementCommand,
    ) -> Result<(), astra_core::ClassifiedError> {
        let command_tx = self.command_tx.take().ok_or_else(|| {
            contract_error("settlement", "non-streaming invocation already settled")
        })?;
        command_tx.send(command).map_err(|_| {
            contract_error(
                "settlement",
                "non-streaming settlement owner stopped before receiving its terminal",
            )
        })?;
        self.task.await.map_err(|error| {
            contract_error(
                "settlement",
                format!("non-streaming settlement owner failed: {error}"),
            )
        })?
    }
}

pub(crate) struct DurableInferenceInvocation {
    shared_pool: SharedPool,
    plan: astra_services::InferenceInvocationPlan,
    observer: Arc<DurableProviderAttemptObserver>,
}

impl DurableInferenceInvocation {
    pub(crate) fn attempt_observer(&self) -> &dyn ProviderAttemptObserver {
        self.observer.as_ref()
    }

    pub(crate) fn attempt_observer_arc(&self) -> Arc<dyn ProviderAttemptObserver> {
        self.observer.clone()
    }

    /// Close every physical attempt that was admitted but has not reached a
    /// durable terminal yet.
    ///
    /// This is used by the client-disconnect supervisor after the response
    /// stream itself has been dropped. The supervisor cannot claim that the
    /// provider did not receive the request, so callers must pass a
    /// `delivery_unknown` terminal rather than inventing success or failure.
    pub(crate) async fn finish_open_attempts(
        &self,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError> {
        self.observer.finish_open_attempts(terminal).await
    }

    /// Converge a logical invocation after its response consumer disappears.
    ///
    /// An open provider attempt is necessarily delivery-unknown. If the
    /// physical attempt already reached a durable terminal, preserve that exact
    /// terminal (including usage and response id). If provider I/O never began,
    /// the logical invocation is simply cancelled.
    pub(crate) async fn finish_after_disconnect(
        &self,
        delivery_unknown: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError> {
        let logical_terminal = self
            .observer
            .terminal_after_disconnect(delivery_unknown)
            .await?;
        self.finish(&logical_terminal).await
    }

    pub(crate) async fn finish_result(
        &self,
        result: &LlmCallResult,
    ) -> Result<(), astra_core::ClassifiedError> {
        self.finish(&terminal_from_result(result)).await
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

#[async_trait]
impl NonstreamInvocationSettlement for DurableInferenceInvocation {
    async fn settle_terminal(
        &self,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError> {
        self.finish(terminal).await
    }

    async fn settle_caller_drop(&self) -> Result<(), astra_core::ClassifiedError> {
        let delivery_unknown = terminal_from_error(&astra_core::ClassifiedError::new(
            astra_core::ErrorKind::StreamTransport,
            "Non-streaming inference caller stopped after durable admission",
        ));
        self.finish_after_disconnect(&delivery_unknown).await
    }
}

struct DurableProviderAttemptObserver {
    shared_pool: SharedPool,
    invocation: astra_services::InferenceInvocationPlan,
    next_attempt: AtomicU32,
    state: tokio::sync::Mutex<ProviderAttemptState>,
}

#[derive(Default)]
struct ProviderAttemptState {
    open_attempts: BTreeSet<u32>,
    latest_terminal: Option<astra_services::InferenceInvocationTerminal>,
}

impl DurableProviderAttemptObserver {
    fn new(shared_pool: SharedPool, invocation: astra_services::InferenceInvocationPlan) -> Self {
        Self {
            shared_pool,
            invocation,
            next_attempt: AtomicU32::new(0),
            state: tokio::sync::Mutex::new(ProviderAttemptState::default()),
        }
    }

    async fn finish_open_attempts(
        &self,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError> {
        let mut state = self.state.lock().await;
        let attempts = state.open_attempts.iter().copied().collect::<Vec<_>>();
        for attempt_index in attempts {
            let attempt =
                astra_services::plan_inference_provider_attempt(&self.invocation, attempt_index);
            astra_services::finish_inference_provider_attempt(
                &self.shared_pool,
                &attempt,
                terminal,
            )
            .await
            .map_err(|error| service_error("provider attempt terminal commit", error))?;
            state.open_attempts.remove(&attempt_index);
            state.latest_terminal = Some(terminal.clone());
        }
        Ok(())
    }

    async fn terminal_after_disconnect(
        &self,
        delivery_unknown: &astra_services::InferenceInvocationTerminal,
    ) -> Result<astra_services::InferenceInvocationTerminal, astra_core::ClassifiedError> {
        let mut state = self.state.lock().await;
        let attempts = state.open_attempts.iter().copied().collect::<Vec<_>>();
        if !attempts.is_empty() {
            for attempt_index in attempts {
                let attempt = astra_services::plan_inference_provider_attempt(
                    &self.invocation,
                    attempt_index,
                );
                astra_services::finish_inference_provider_attempt(
                    &self.shared_pool,
                    &attempt,
                    delivery_unknown,
                )
                .await
                .map_err(|error| service_error("provider attempt terminal commit", error))?;
                state.open_attempts.remove(&attempt_index);
            }
            state.latest_terminal = Some(delivery_unknown.clone());
            return Ok(delivery_unknown.clone());
        }

        Ok(state.latest_terminal.clone().unwrap_or_else(|| {
            terminal_from_error(&astra_core::ClassifiedError::new(
                astra_core::ErrorKind::Cancelled,
                "Inference cancelled before provider delivery",
            ))
        }))
    }
}

#[async_trait]
impl ProviderAttemptObserver for DurableProviderAttemptObserver {
    async fn begin_attempt(&self) -> Result<u32, astra_core::ClassifiedError> {
        // Serialize admission with disconnect cleanup. Once this method
        // returns, the attempt is both durable and visible in `open_attempts`;
        // cleanup can therefore never miss the window between those facts.
        let mut state = self.state.lock().await;
        let attempt_index = self.next_attempt.fetch_add(1, Ordering::AcqRel);
        let attempt =
            astra_services::plan_inference_provider_attempt(&self.invocation, attempt_index);
        astra_services::begin_inference_provider_attempt(&self.shared_pool, &attempt)
            .await
            .map_err(|error| service_error("provider attempt admission", error))?;
        state.open_attempts.insert(attempt_index);
        Ok(attempt_index)
    }

    async fn finish_attempt(
        &self,
        attempt_index: u32,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError> {
        // The same lock makes a normal stream terminal and disconnect cleanup
        // mutually exclusive. The durable service remains idempotent for a
        // repeated identical terminal, but two owners cannot race different
        // terminal claims through this in-process observer.
        let mut state = self.state.lock().await;
        let attempt =
            astra_services::plan_inference_provider_attempt(&self.invocation, attempt_index);
        astra_services::finish_inference_provider_attempt(&self.shared_pool, &attempt, terminal)
            .await
            .map_err(|error| service_error("provider attempt terminal commit", error))?;
        state.open_attempts.remove(&attempt_index);
        state.latest_terminal = Some(terminal.clone());
        Ok(())
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
        astra_core::ErrorKind::StreamIdle | astra_core::ErrorKind::StreamTransport => {
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

fn terminal_from_result(result: &LlmCallResult) -> astra_services::InferenceInvocationTerminal {
    let usage = crate::turn::token_usage::TokenUsage::from_partial_json_map(&result.usage);
    astra_services::InferenceInvocationTerminal::succeeded(
        astra_services::InferenceUsage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cached_input_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
        },
        result.response_id.clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingSettlementOwner {
        dropped: tokio::sync::Notify,
        drop_count: AtomicU32,
    }

    #[async_trait]
    impl NonstreamInvocationSettlement for RecordingSettlementOwner {
        async fn settle_terminal(
            &self,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> Result<(), astra_core::ClassifiedError> {
            Ok(())
        }

        async fn settle_caller_drop(&self) -> Result<(), astra_core::ClassifiedError> {
            self.drop_count.fetch_add(1, Ordering::AcqRel);
            self.dropped.notify_one();
            Ok(())
        }
    }

    #[tokio::test]
    async fn nonstream_settlement_outlives_a_dropped_caller() {
        let owner = Arc::new(RecordingSettlementOwner::default());
        let supervisor = NonstreamInvocationSupervisor::start(owner.clone());

        drop(supervisor);

        tokio::time::timeout(std::time::Duration::from_secs(1), owner.dropped.notified())
            .await
            .expect("dropped caller must wake the independent settlement owner");
        assert_eq!(owner.drop_count.load(Ordering::Acquire), 1);
    }

    struct FailingTerminalSettlementOwner;

    #[async_trait]
    impl NonstreamInvocationSettlement for FailingTerminalSettlementOwner {
        async fn settle_terminal(
            &self,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> Result<(), astra_core::ClassifiedError> {
            Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::DatabaseError,
                "terminal commit rejected",
            ))
        }

        async fn settle_caller_drop(&self) -> Result<(), astra_core::ClassifiedError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn nonstream_settlement_failure_is_returned_to_the_caller() {
        let supervisor =
            NonstreamInvocationSupervisor::start(Arc::new(FailingTerminalSettlementOwner));
        let terminal = astra_services::InferenceInvocationTerminal::succeeded(
            astra_services::InferenceUsage::default(),
            None,
        );

        let error = supervisor
            .settle(NonstreamSettlementCommand::Terminal(terminal))
            .await
            .expect_err("a logical terminal commit failure must fail the call");

        assert_eq!(error.kind, astra_core::ErrorKind::DatabaseError);
        assert_eq!(error.message, "terminal commit rejected");
    }

    #[test]
    fn terminal_status_distinguishes_pre_delivery_failure_from_uncertain_delivery() {
        let connect_failure = terminal_from_error(&astra_core::ClassifiedError::new(
            astra_core::ErrorKind::Network,
            "connection refused",
        ));
        let stream_failure = terminal_from_error(&astra_core::ClassifiedError::new(
            astra_core::ErrorKind::StreamTransport,
            "connection reset after delivery",
        ));

        assert_eq!(
            connect_failure.status,
            astra_services::InferenceTerminalStatus::Failed
        );
        assert_eq!(
            stream_failure.status,
            astra_services::InferenceTerminalStatus::DeliveryUnknown
        );
    }
}
