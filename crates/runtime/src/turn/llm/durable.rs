//! Runtime boundary between admitted model execution and the durable inference ledger.
//!
//! Callers own semantic purpose and causal scope. This module owns the invariant
//! that route/invocation admission happens before provider I/O, every physical
//! request is observed, and the logical terminal matches the provider terminal.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use async_trait::async_trait;
use serde_json::json;

use crate::turn::llm::client::{
    LlmCall, LlmCallResult, ProviderAttemptObserver, ProviderWireRequestIdentity,
};
use astra_core::SharedPool;

const INFERENCE_LEDGER_ERROR_SOURCE: &str = "inference_execution_ledger";

#[async_trait]
pub(crate) trait InferenceLedgerPersistence: Send + Sync {
    async fn admit_invocation(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
    ) -> astra_services::ServiceResult<()>;

    async fn declare_settlement(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<()>;

    async fn finish_invocation(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<()>;

    async fn begin_provider_attempt(
        &self,
        attempt: &astra_services::InferenceProviderAttemptPlan,
    ) -> astra_services::ServiceResult<()>;

    async fn finish_provider_attempt(
        &self,
        attempt: &astra_services::InferenceProviderAttemptPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<()>;
}

struct DatabaseInferenceLedgerPersistence {
    shared_pool: SharedPool,
}

#[async_trait]
impl InferenceLedgerPersistence for DatabaseInferenceLedgerPersistence {
    async fn admit_invocation(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
    ) -> astra_services::ServiceResult<()> {
        astra_services::admit_inference_invocation(&self.shared_pool, plan).await
    }

    async fn declare_settlement(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<()> {
        astra_services::declare_inference_settlement(&self.shared_pool, plan, terminal).await
    }

    async fn finish_invocation(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<()> {
        astra_services::finish_inference_invocation(&self.shared_pool, plan, terminal).await
    }

    async fn begin_provider_attempt(
        &self,
        attempt: &astra_services::InferenceProviderAttemptPlan,
    ) -> astra_services::ServiceResult<()> {
        astra_services::begin_inference_provider_attempt(&self.shared_pool, attempt).await
    }

    async fn finish_provider_attempt(
        &self,
        attempt: &astra_services::InferenceProviderAttemptPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<()> {
        astra_services::finish_inference_provider_attempt(&self.shared_pool, attempt, terminal)
            .await
    }
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct TestInferenceLedgerPersistence {
    state: Arc<std::sync::Mutex<TestInferenceLedgerState>>,
}

#[cfg(test)]
#[derive(Default)]
struct TestInferenceLedgerState {
    invocations: BTreeMap<String, TestInvocationState>,
    attempts: BTreeMap<String, TestProviderAttemptState>,
}

#[cfg(test)]
#[derive(Default)]
struct TestInvocationState {
    settlement: Option<astra_services::InferenceInvocationTerminal>,
    terminal: Option<astra_services::InferenceInvocationTerminal>,
}

#[cfg(test)]
struct TestProviderAttemptState {
    invocation_id: String,
    terminal: Option<astra_services::InferenceInvocationTerminal>,
}

#[cfg(test)]
impl TestInferenceLedgerPersistence {
    fn lock(&self) -> std::sync::MutexGuard<'_, TestInferenceLedgerState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn assert_quiescent(&self) {
        let state = self.lock();
        assert!(
            state
                .invocations
                .values()
                .all(|invocation| invocation.terminal.is_some()),
            "every admitted test invocation must have one logical terminal"
        );
        assert!(
            state
                .attempts
                .values()
                .all(|attempt| attempt.terminal.is_some()),
            "every admitted test provider attempt must have one terminal"
        );
    }
}

#[cfg(test)]
#[async_trait]
impl InferenceLedgerPersistence for TestInferenceLedgerPersistence {
    async fn admit_invocation(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
    ) -> astra_services::ServiceResult<()> {
        let mut state = self.lock();
        if state
            .invocations
            .insert(
                plan.invocation_id().to_string(),
                TestInvocationState::default(),
            )
            .is_some()
        {
            return Err(astra_services::ServiceError::conflict(format!(
                "test inference invocation {} was admitted twice",
                plan.invocation_id()
            )));
        }
        Ok(())
    }

    async fn declare_settlement(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<()> {
        let mut state = self.lock();
        let invocation = state
            .invocations
            .get_mut(plan.invocation_id())
            .ok_or_else(|| {
                astra_services::ServiceError::conflict(format!(
                    "test inference invocation {} was not admitted",
                    plan.invocation_id()
                ))
            })?;
        match invocation.settlement.as_ref() {
            Some(existing) if existing != terminal => {
                return Err(astra_services::ServiceError::conflict(format!(
                    "test inference invocation {} has a conflicting settlement",
                    plan.invocation_id()
                )));
            }
            Some(_) => {}
            None => invocation.settlement = Some(terminal.clone()),
        }
        Ok(())
    }

    async fn finish_invocation(
        &self,
        plan: &astra_services::InferenceInvocationPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<()> {
        let mut state = self.lock();
        if state.attempts.values().any(|attempt| {
            attempt.invocation_id == plan.invocation_id() && attempt.terminal.is_none()
        }) {
            return Err(astra_services::ServiceError::conflict(format!(
                "test inference invocation {} still has an open provider attempt",
                plan.invocation_id()
            )));
        }
        if terminal.status == astra_services::InferenceTerminalStatus::Succeeded
            && !state.attempts.values().any(|attempt| {
                attempt.invocation_id == plan.invocation_id()
                    && attempt.terminal.as_ref() == Some(terminal)
            })
        {
            return Err(astra_services::ServiceError::conflict(format!(
                "test inference invocation {} has no matching successful provider terminal",
                plan.invocation_id()
            )));
        }
        let invocation = state
            .invocations
            .get_mut(plan.invocation_id())
            .ok_or_else(|| {
                astra_services::ServiceError::conflict(format!(
                    "test inference invocation {} was not admitted",
                    plan.invocation_id()
                ))
            })?;
        match invocation.terminal.as_ref() {
            Some(existing) if existing != terminal => {
                return Err(astra_services::ServiceError::conflict(format!(
                    "test inference invocation {} has a conflicting terminal",
                    plan.invocation_id()
                )));
            }
            Some(_) => {}
            None => invocation.terminal = Some(terminal.clone()),
        }
        Ok(())
    }

    async fn begin_provider_attempt(
        &self,
        attempt: &astra_services::InferenceProviderAttemptPlan,
    ) -> astra_services::ServiceResult<()> {
        let mut state = self.lock();
        let invocation = state
            .invocations
            .get(attempt.invocation_id())
            .ok_or_else(|| {
                astra_services::ServiceError::conflict(format!(
                    "test provider attempt {} has no admitted invocation",
                    attempt.attempt_id()
                ))
            })?;
        if invocation.settlement.is_some() || invocation.terminal.is_some() {
            return Err(astra_services::ServiceError::conflict(format!(
                "test provider attempt {} started after settlement",
                attempt.attempt_id()
            )));
        }
        if state
            .attempts
            .insert(
                attempt.attempt_id().to_string(),
                TestProviderAttemptState {
                    invocation_id: attempt.invocation_id().to_string(),
                    terminal: None,
                },
            )
            .is_some()
        {
            return Err(astra_services::ServiceError::conflict(format!(
                "test provider attempt {} was admitted twice",
                attempt.attempt_id()
            )));
        }
        Ok(())
    }

    async fn finish_provider_attempt(
        &self,
        attempt: &astra_services::InferenceProviderAttemptPlan,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> astra_services::ServiceResult<()> {
        let mut state = self.lock();
        let attempt_state = state
            .attempts
            .get_mut(attempt.attempt_id())
            .ok_or_else(|| {
                astra_services::ServiceError::conflict(format!(
                    "test provider attempt {} was not admitted",
                    attempt.attempt_id()
                ))
            })?;
        match attempt_state.terminal.as_ref() {
            Some(existing) if existing != terminal => {
                return Err(astra_services::ServiceError::conflict(format!(
                    "test provider attempt {} has a conflicting terminal",
                    attempt.attempt_id()
                )));
            }
            Some(_) => {}
            None => attempt_state.terminal = Some(terminal.clone()),
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct DurableInferenceLedger {
    persistence: Arc<dyn InferenceLedgerPersistence>,
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
            persistence: Arc::new(DatabaseInferenceLedgerPersistence { shared_pool }),
            user_id: user_id.into(),
            admitted_execution,
        }
    }

    pub(crate) fn required(
        shared_pool: Option<&SharedPool>,
        admitted_execution: Option<&astra_services::AdmittedModelExecution>,
        user_id: &str,
    ) -> Result<Self, astra_core::ClassifiedError> {
        Self::required_with_persistence(shared_pool, admitted_execution, user_id, None)
    }

    pub(crate) fn required_with_persistence(
        shared_pool: Option<&SharedPool>,
        admitted_execution: Option<&astra_services::AdmittedModelExecution>,
        user_id: &str,
        persistence: Option<Arc<dyn InferenceLedgerPersistence>>,
    ) -> Result<Self, astra_core::ClassifiedError> {
        let persistence = match persistence {
            Some(persistence) => persistence,
            None => {
                let shared_pool = shared_pool.ok_or_else(|| {
                    contract_error(
                        "admission",
                        "Server execution has no durable inference database",
                    )
                })?;
                Arc::new(DatabaseInferenceLedgerPersistence {
                    shared_pool: shared_pool.clone(),
                })
            }
        };
        let admitted_execution = admitted_execution.ok_or_else(|| {
            contract_error(
                "admission",
                "Server execution has no admitted Offering material",
            )
        })?;
        Ok(Self {
            persistence,
            user_id: user_id.to_string(),
            admitted_execution: admitted_execution.clone(),
        })
    }

    pub(crate) async fn admit(
        &self,
        scope: astra_turn_types::InferenceInvocationScope,
        purpose: astra_turn_types::InferencePurpose,
        resolved_model_name: &str,
        upstream_model_name: &str,
        provider: &str,
    ) -> Result<DurableInferenceInvocation, astra_core::ClassifiedError> {
        let mut request_context = astra_services::ModelRequestContextSeed::server_default();
        if self.admitted_execution.execution_placement
            == astra_services::ModelExecutionPlacement::Edge
        {
            request_context.topology = astra_services::ModelRequestTopology::EdgeServer;
            request_context.execution_binding = "edge".to_string();
        }
        self.admit_with_request_context(
            scope,
            purpose,
            resolved_model_name,
            upstream_model_name,
            provider,
            request_context,
        )
        .await
    }

    pub(crate) async fn admit_with_request_context(
        &self,
        scope: astra_turn_types::InferenceInvocationScope,
        purpose: astra_turn_types::InferencePurpose,
        resolved_model_name: &str,
        upstream_model_name: &str,
        provider: &str,
        request_context: astra_services::ModelRequestContextSeed,
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
        let request_context = normalize_request_context_for_execution(
            request_context,
            self.admitted_execution.execution_placement,
        );
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
        self.persistence
            .admit_invocation(&plan)
            .await
            .map_err(|error| service_error("admission", error))?;
        Ok(DurableInferenceInvocation {
            observer: Arc::new(DurableProviderAttemptObserver::new_with_persistence(
                self.persistence.clone(),
                plan.clone(),
                request_context,
            )),
            persistence: self.persistence.clone(),
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

fn normalize_request_context_for_execution(
    mut context: astra_services::ModelRequestContextSeed,
    placement: astra_services::ModelExecutionPlacement,
) -> astra_services::ModelRequestContextSeed {
    context.execution_binding = match placement {
        astra_services::ModelExecutionPlacement::Server => "server",
        astra_services::ModelExecutionPlacement::Edge => "edge",
    }
    .to_string();
    if placement == astra_services::ModelExecutionPlacement::Edge
        && context.topology == astra_services::ModelRequestTopology::ServerOnly
    {
        context.topology = astra_services::ModelRequestTopology::EdgeServer;
        context.interaction_owner = "edge".to_string();
        context.loop_owner = "server".to_string();
    }
    context
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
    persistence: Arc<dyn InferenceLedgerPersistence>,
    plan: astra_services::InferenceInvocationPlan,
    observer: Arc<DurableProviderAttemptObserver>,
}

/// Exact identity of the latest durably admitted physical provider request.
///
/// This is the bridge between transport-owned serialized bytes and the
/// turn-level context trace. It is populated only after the attempt row
/// commits, so a trace can never claim that an unadmitted request was sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DurableProviderRequestIdentity {
    pub request_id: String,
    pub request_hash: String,
    pub attempt: u32,
    pub protocol: crate::turn::llm::client::LlmProviderProtocol,
    pub provider_wire_bytes: u64,
    pub composition: crate::turn::llm::client::ProviderWireComposition,
}

/// One admitted physical request and its terminal fact, when observed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DurableProviderAttemptFact {
    pub request: DurableProviderRequestIdentity,
    pub terminal: Option<astra_services::InferenceInvocationTerminal>,
}

impl DurableInferenceInvocation {
    pub(crate) fn attempt_observer(&self) -> &dyn ProviderAttemptObserver {
        self.observer.as_ref()
    }

    pub(crate) fn attempt_observer_arc(&self) -> Arc<dyn ProviderAttemptObserver> {
        self.observer.clone()
    }

    pub(crate) async fn provider_attempt_facts(&self) -> Vec<DurableProviderAttemptFact> {
        self.observer.state.lock().await.attempt_facts()
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
        self.persistence
            .declare_settlement(&self.plan, terminal)
            .await
            .map_err(|error| service_error("settlement declaration", error))?;
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
        self.finish_after_disconnect_with_partial_provider_facts(
            delivery_unknown,
            astra_services::InferenceUsage::default(),
            None,
        )
        .await
    }

    pub(crate) async fn finish_after_disconnect_with_partial_provider_facts(
        &self,
        delivery_unknown: &astra_services::InferenceInvocationTerminal,
        usage: astra_services::InferenceUsage,
        provider_response_id: Option<String>,
    ) -> Result<(), astra_core::ClassifiedError> {
        let mut delivery_unknown = delivery_unknown.clone();
        delivery_unknown.usage = usage;
        delivery_unknown.provider_response_id = provider_response_id;
        let logical_terminal = self
            .observer
            .terminal_after_disconnect(&delivery_unknown)
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
        self.finish_error_with_partial_provider_facts(
            error,
            astra_services::InferenceUsage::default(),
            None,
        )
        .await
    }

    pub(crate) async fn finish_error_with_partial_provider_facts(
        &self,
        error: &astra_core::ClassifiedError,
        usage: astra_services::InferenceUsage,
        provider_response_id: Option<String>,
    ) -> Result<(), astra_core::ClassifiedError> {
        let mut fallback = if is_ledger_error(error) {
            unsettled_attempt_terminal()
        } else {
            terminal_from_error(error)
        };
        fallback.usage = usage;
        fallback.provider_response_id = provider_response_id;
        let observed_terminal = {
            let state = self.observer.state.lock().await;
            state.quiescent_terminal()
        };
        if let Some(terminal) = observed_terminal {
            // The transport already committed the physical terminal before
            // surfacing the error to its caller. Preserve its measured usage
            // and provider response identity instead of replacing those facts
            // with a zero-usage wrapper error.
            return self.finish(&terminal).await;
        }
        self.finish_open_attempts(&fallback).await?;
        self.finish(&fallback).await
    }

    pub(crate) async fn finish(
        &self,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError> {
        self.persistence
            .finish_invocation(&self.plan, terminal)
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
    persistence: Arc<dyn InferenceLedgerPersistence>,
    invocation: astra_services::InferenceInvocationPlan,
    request_context: astra_services::ModelRequestContextSeed,
    next_attempt: AtomicU32,
    state: Arc<tokio::sync::Mutex<ProviderAttemptState>>,
    operations: ProviderOperationGate,
}

#[derive(Default)]
struct ProviderAttemptState {
    open_attempts: BTreeMap<u32, astra_services::InferenceProviderAttemptPlan>,
    requests: BTreeMap<u32, DurableProviderRequestIdentity>,
    terminals: BTreeMap<u32, astra_services::InferenceInvocationTerminal>,
}

impl ProviderAttemptState {
    fn attempt_facts(&self) -> Vec<DurableProviderAttemptFact> {
        self.requests
            .iter()
            .map(|(attempt, request)| DurableProviderAttemptFact {
                request: request.clone(),
                terminal: self.terminals.get(attempt).cloned(),
            })
            .collect()
    }

    fn quiescent_terminal(&self) -> Option<astra_services::InferenceInvocationTerminal> {
        self.open_attempts
            .is_empty()
            .then(|| {
                self.terminals
                    .last_key_value()
                    .map(|(_, terminal)| terminal.clone())
            })
            .flatten()
    }
}

#[derive(Clone, Default)]
struct ProviderOperationGate {
    state: Arc<std::sync::Mutex<ProviderOperationGateState>>,
    drained: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
struct ProviderOperationGateState {
    closed: bool,
    active: u32,
}

struct ProviderOperationPermit {
    gate: ProviderOperationGate,
}

impl ProviderOperationGate {
    fn register(
        &self,
        stage: &'static str,
    ) -> Result<ProviderOperationPermit, astra_core::ClassifiedError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return Err(contract_error(
                stage,
                "provider attempt settlement has already started",
            ));
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| contract_error(stage, "provider operation count overflow"))?;
        Ok(ProviderOperationPermit { gate: self.clone() })
    }

    async fn close_and_wait(&self) {
        loop {
            let drained = self.drained.notified();
            tokio::pin!(drained);
            // `notified()` is lazy. Explicitly enable the pinned waiter before
            // inspecting the count so the final permit cannot drop in the
            // check-to-first-poll window and lose `notify_waiters()`.
            drained.as_mut().enable();
            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.closed = true;
                if state.active == 0 {
                    return;
                }
            }
            drained.await;
        }
    }
}

impl Drop for ProviderOperationPermit {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active = state
            .active
            .checked_sub(1)
            .expect("provider operation permits have one matching registration");
        if state.active == 0 {
            self.gate.drained.notify_waiters();
        }
    }
}

async fn finish_attempt_batch<T, F, Fut>(
    attempts: Vec<T>,
    mut finish: F,
) -> (Vec<T>, Option<astra_core::ClassifiedError>)
where
    T: Clone + std::fmt::Debug,
    F: FnMut(T) -> Fut,
    Fut: std::future::Future<Output = Result<(), astra_core::ClassifiedError>>,
{
    let mut completed = Vec::with_capacity(attempts.len());
    let mut first_error = None;
    for attempt in attempts {
        match finish(attempt.clone()).await {
            Ok(()) => completed.push(attempt),
            Err(error) => {
                astra_core::agent_error!(
                    "llm",
                    "provider attempt {attempt:?} terminal commit failed: {error}"
                );
                first_error.get_or_insert(error);
            }
        }
    }
    (completed, first_error)
}

impl DurableProviderAttemptObserver {
    fn new_with_persistence(
        persistence: Arc<dyn InferenceLedgerPersistence>,
        invocation: astra_services::InferenceInvocationPlan,
        request_context: astra_services::ModelRequestContextSeed,
    ) -> Self {
        Self {
            persistence,
            invocation,
            request_context,
            next_attempt: AtomicU32::new(0),
            state: Arc::new(tokio::sync::Mutex::new(ProviderAttemptState::default())),
            operations: ProviderOperationGate::default(),
        }
    }

    async fn finish_open_attempts(
        &self,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError> {
        self.operations.close_and_wait().await;
        let mut state = self.state.lock().await;
        let attempts = state
            .open_attempts
            .iter()
            .map(|(index, attempt)| (*index, attempt.clone()))
            .collect::<Vec<_>>();
        let (completed, first_error) =
            finish_attempt_batch(attempts, |(_attempt_index, attempt)| async move {
                self.persistence
                    .finish_provider_attempt(&attempt, terminal)
                    .await
                    .map_err(|error| service_error("provider attempt terminal commit", error))
            })
            .await;
        for (attempt_index, _) in completed {
            state.open_attempts.remove(&attempt_index);
            state.terminals.insert(attempt_index, terminal.clone());
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    async fn terminal_after_disconnect(
        &self,
        delivery_unknown: &astra_services::InferenceInvocationTerminal,
    ) -> Result<astra_services::InferenceInvocationTerminal, astra_core::ClassifiedError> {
        self.operations.close_and_wait().await;
        let mut state = self.state.lock().await;
        let attempts = state
            .open_attempts
            .iter()
            .map(|(index, attempt)| (*index, attempt.clone()))
            .collect::<Vec<_>>();
        if !attempts.is_empty() {
            self.persistence
                .declare_settlement(&self.invocation, delivery_unknown)
                .await
                .map_err(|error| service_error("disconnect settlement declaration", error))?;
            let (completed, first_error) =
                finish_attempt_batch(attempts, |(_attempt_index, attempt)| async move {
                    self.persistence
                        .finish_provider_attempt(&attempt, delivery_unknown)
                        .await
                        .map_err(|error| service_error("provider attempt terminal commit", error))
                })
                .await;
            for (attempt_index, _) in completed {
                state.open_attempts.remove(&attempt_index);
                state
                    .terminals
                    .insert(attempt_index, delivery_unknown.clone());
            }
            if let Some(error) = first_error {
                return Err(error);
            }
            return Ok(delivery_unknown.clone());
        }

        Ok(state
            .terminals
            .last_key_value()
            .map(|(_, terminal)| terminal.clone())
            .unwrap_or_else(|| {
                terminal_from_error(&astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::Cancelled,
                    "Inference cancelled before provider delivery",
                ))
            }))
    }
}

#[async_trait]
impl ProviderAttemptObserver for DurableProviderAttemptObserver {
    async fn begin_attempt(
        &self,
        wire: &ProviderWireRequestIdentity,
    ) -> Result<u32, astra_core::ClassifiedError> {
        // The permit is registered synchronously before the first await.
        // Dropping the caller only detaches the task; it cannot cancel a DB
        // commit between durable admission and the in-memory state update.
        // Disconnect cleanup closes the gate and waits for every such task.
        let permit = self.operations.register("provider attempt admission")?;
        let attempt_index = self.next_attempt.fetch_add(1, Ordering::AcqRel);
        let service_wire = astra_services::InferenceProviderWireIdentity::new(
            wire.protocol.as_str(),
            wire.provider_wire_hash.clone(),
            wire.provider_wire_bytes,
        )
        .map_err(|error| service_error("provider wire identity", error))?
        .with_composition(astra_services::ModelRequestWireComposition {
            system_bytes: wire.composition.system_bytes,
            conversation_bytes: wire.composition.conversation_bytes,
            tool_schema_bytes: wire.composition.tool_schema_bytes,
            provider_envelope_bytes: wire.composition.provider_envelope_bytes,
            system_items: wire.composition.system_items,
            conversation_items: wire.composition.conversation_items,
            tool_schema_items: wire.composition.tool_schema_items,
        });
        let attempt = astra_services::plan_inference_provider_attempt_with_context(
            &self.invocation,
            attempt_index,
            service_wire,
            self.request_context.clone(),
        );
        let request = DurableProviderRequestIdentity {
            request_id: attempt.request_id().to_string(),
            request_hash: wire.provider_wire_hash.clone(),
            attempt: attempt_index,
            protocol: wire.protocol,
            provider_wire_bytes: wire.provider_wire_bytes,
            composition: wire.composition.clone(),
        };
        let persistence = self.persistence.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let mut state = state.lock().await;
            persistence
                .begin_provider_attempt(&attempt)
                .await
                .map_err(|error| service_error("provider attempt admission", error))?;
            state.requests.insert(attempt_index, request);
            state.open_attempts.insert(attempt_index, attempt);
            Ok(attempt_index)
        })
        .await
        .map_err(|error| {
            contract_error(
                "provider attempt admission",
                format!("detached durable operation failed: {error}"),
            )
        })?
    }

    async fn finish_attempt(
        &self,
        attempt_index: u32,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError> {
        // As with admission, the detached task is the sole owner of the
        // durable commit and the matching state transition.
        let permit = self.operations.register("provider attempt terminal")?;
        let persistence = self.persistence.clone();
        let state = self.state.clone();
        let terminal = terminal.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let mut state = state.lock().await;
            let attempt = state
                .open_attempts
                .get(&attempt_index)
                .cloned()
                .ok_or_else(|| {
                    contract_error(
                        "provider attempt terminal",
                        format!("attempt {attempt_index} is not open"),
                    )
                })?;
            persistence
                .finish_provider_attempt(&attempt, &terminal)
                .await
                .map_err(|error| service_error("provider attempt terminal commit", error))?;
            state.open_attempts.remove(&attempt_index);
            state.terminals.insert(attempt_index, terminal);
            Ok(())
        })
        .await
        .map_err(|error| {
            contract_error(
                "provider attempt terminal",
                format!("detached durable operation failed: {error}"),
            )
        })?
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

fn unsettled_attempt_terminal() -> astra_services::InferenceInvocationTerminal {
    astra_services::InferenceInvocationTerminal {
        status: astra_services::InferenceTerminalStatus::DeliveryUnknown,
        usage: astra_services::InferenceUsage::default(),
        provider_response_id: None,
        error_kind: Some("inference_ledger".to_string()),
        error_message: Some(
            "provider attempt terminal state could not be committed durably".to_string(),
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

    #[test]
    fn execution_placement_is_normalized_independently_from_surface_topology() {
        let cli_edge = normalize_request_context_for_execution(
            astra_services::ModelRequestContextSeed {
                topology: astra_services::ModelRequestTopology::CliServer,
                interaction_owner: "cli".to_string(),
                loop_owner: "server".to_string(),
                ..astra_services::ModelRequestContextSeed::server_default()
            },
            astra_services::ModelExecutionPlacement::Edge,
        );
        assert_eq!(
            cli_edge.topology,
            astra_services::ModelRequestTopology::CliServer
        );
        assert_eq!(cli_edge.interaction_owner, "cli");
        assert_eq!(cli_edge.loop_owner, "server");
        assert_eq!(cli_edge.execution_binding, "edge");

        let server_edge = normalize_request_context_for_execution(
            astra_services::ModelRequestContextSeed::server_default(),
            astra_services::ModelExecutionPlacement::Edge,
        );
        assert_eq!(
            server_edge.topology,
            astra_services::ModelRequestTopology::EdgeServer
        );
        assert_eq!(server_edge.interaction_owner, "edge");
        assert_eq!(server_edge.loop_owner, "server");
        assert_eq!(server_edge.execution_binding, "edge");

        let edge_server = normalize_request_context_for_execution(
            server_edge,
            astra_services::ModelExecutionPlacement::Server,
        );
        assert_eq!(
            edge_server.topology,
            astra_services::ModelRequestTopology::EdgeServer
        );
        assert_eq!(edge_server.execution_binding, "server");
    }

    #[test]
    fn required_ledger_fails_closed_without_a_database() {
        let error = match DurableInferenceLedger::required(None, None, "user-7") {
            Ok(_) => panic!("real provider execution must not bypass durable attempt admission"),
            Err(error) => error,
        };

        assert_eq!(error.kind, astra_core::ErrorKind::ContractViolation);
        assert!(
            error.message.contains("no durable inference database"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn closing_operation_gate_waits_for_registered_detached_work() {
        let gate = ProviderOperationGate::default();
        let permit = gate
            .register("test operation")
            .expect("first operation is admitted");
        let release = Arc::new(tokio::sync::Notify::new());
        let released = release.clone();
        let detached = tokio::spawn(async move {
            released.notified().await;
            drop(permit);
        });

        let closing_gate = gate.clone();
        let mut closing = tokio::spawn(async move {
            closing_gate.close_and_wait().await;
        });
        tokio::task::yield_now().await;

        assert!(
            !closing.is_finished(),
            "cleanup must wait for the already registered durable operation"
        );
        assert!(
            gate.register("late operation").is_err(),
            "closing the gate must synchronously reject new provider operations"
        );

        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut closing)
            .await
            .expect("cleanup should finish once detached work drains")
            .expect("cleanup task should not panic");
        detached.await.expect("detached work should not panic");
    }

    #[tokio::test]
    async fn attempt_batch_continues_after_an_individual_terminal_failure() {
        let visited = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let observed = visited.clone();
        let (completed, error) = finish_attempt_batch(vec![0, 1, 2], move |attempt_index| {
            let observed = observed.clone();
            async move {
                observed.lock().await.push(attempt_index);
                if attempt_index == 0 {
                    Err(astra_core::ClassifiedError::new(
                        astra_core::ErrorKind::DatabaseError,
                        "first terminal write failed",
                    ))
                } else {
                    Ok(())
                }
            }
        })
        .await;

        assert_eq!(*visited.lock().await, vec![0, 1, 2]);
        assert_eq!(completed, vec![1, 2]);
        assert_eq!(
            error.expect("the first error remains observable").message,
            "first terminal write failed"
        );
    }

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
        assert_eq!(
            unsettled_attempt_terminal().status,
            astra_services::InferenceTerminalStatus::DeliveryUnknown,
            "a lost durable provider terminal must never be reported as safely retryable"
        );
    }

    #[test]
    fn quiescent_transport_terminal_preserves_partial_provider_facts() {
        let terminal = astra_services::InferenceInvocationTerminal {
            status: astra_services::InferenceTerminalStatus::DeliveryUnknown,
            usage: astra_services::InferenceUsage {
                input_tokens: 200,
                output_tokens: 50,
                cache_read_tokens: 800,
                cache_creation_tokens: 100,
            },
            provider_response_id: Some("provider-response-7".to_string()),
            error_kind: Some("stream_transport".to_string()),
            error_message: Some("partial delivery".to_string()),
        };
        let mut state = ProviderAttemptState::default();
        state.terminals.insert(0, terminal.clone());

        assert_eq!(state.quiescent_terminal(), Some(terminal));
    }

    #[test]
    fn physical_attempt_inventory_keeps_earlier_retries_in_order() {
        let mut state = ProviderAttemptState::default();
        for attempt in [0_u32, 1] {
            state.requests.insert(
                attempt,
                DurableProviderRequestIdentity {
                    request_id: format!("request-{attempt}"),
                    request_hash:
                        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                            .to_string(),
                    attempt,
                    protocol: crate::turn::llm::client::LlmProviderProtocol::OpenAiCompatible,
                    provider_wire_bytes: 128,
                    composition: crate::turn::llm::client::ProviderWireComposition {
                        provider_envelope_bytes: 128,
                        ..Default::default()
                    },
                },
            );
        }
        state.terminals.insert(
            0,
            astra_services::InferenceInvocationTerminal {
                status: astra_services::InferenceTerminalStatus::Failed,
                usage: astra_services::InferenceUsage::default(),
                provider_response_id: Some("provider-429".to_string()),
                error_kind: Some("rate_limit".to_string()),
                error_message: None,
            },
        );

        let facts = state.attempt_facts();
        assert_eq!(
            facts
                .iter()
                .map(|fact| fact.request.request_id.as_str())
                .collect::<Vec<_>>(),
            vec!["request-0", "request-1"]
        );
        assert_eq!(
            facts[0]
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.provider_response_id.as_deref()),
            Some("provider-429")
        );
        assert!(facts[1].terminal.is_none());
    }
}
