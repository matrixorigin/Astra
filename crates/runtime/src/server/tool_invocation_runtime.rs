//! Runtime adapter over the shared durable invocation ledger contract.
//!
//! This module is also the single projection boundary between ephemeral
//! `ToolResult` values and durable typed outcomes. Classification only uses
//! machine-readable fields; provider/user errors are never inferred from
//! human-readable prose.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use astra_turn_core::invocation_ledger::{InMemoryInvocationLedger, InvocationLedgerError};
use astra_turn_types::{
    DispatchCertainty, SemanticReadCacheKey, SemanticReadObservation,
    ToolInvocationCompletionSource, ToolInvocationDecision, ToolInvocationDispatchLease,
    ToolInvocationFingerprint, ToolInvocationIdentity, ToolInvocationPrepareOutcome,
    ToolInvocationRecord, ToolInvocationResultPayload, ToolInvocationState,
    ToolInvocationTerminalOutcome,
};
use serde_json::{Map, Value, json};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

pub(crate) enum InvocationBeginDisposition {
    Execute {
        decision: ToolInvocationDecision,
        owner_id: String,
    },
    Return(astra_tools::ToolResult),
}

pub(crate) enum InvocationPrepareDisposition {
    Prepared { decision: ToolInvocationDecision },
    Return(astra_tools::ToolResult),
}

pub(crate) const DISPATCH_LEASE_DURATION: Duration = Duration::from_secs(90);
pub(crate) const DISPATCH_LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(30);

pub(crate) struct DispatchLeaseHeartbeat {
    cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl DispatchLeaseHeartbeat {
    pub(crate) async fn stop(mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                tracing::warn!(%error, "tool invocation lease heartbeat task failed");
            }
        }
    }
}

impl Drop for DispatchLeaseHeartbeat {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Clone)]
pub(crate) enum RuntimeToolInvocationLedger {
    Database(astra_services::tool_invocation_ledger::DatabaseToolInvocationLedger),
    InMemory(Arc<tokio::sync::Mutex<InMemoryInvocationLedger>>),
}

impl RuntimeToolInvocationLedger {
    pub(crate) fn new(pool: Option<astra_core::SharedPool>) -> Self {
        match pool {
            Some(pool) => Self::Database(
                astra_services::tool_invocation_ledger::DatabaseToolInvocationLedger::new(pool),
            ),
            None => Self::InMemory(Arc::new(tokio::sync::Mutex::new(
                InMemoryInvocationLedger::default(),
            ))),
        }
    }

    pub(crate) async fn prepare(
        &self,
        identity: &ToolInvocationIdentity,
        fingerprint: &ToolInvocationFingerprint,
        decision: &ToolInvocationDecision,
    ) -> Result<ToolInvocationPrepareOutcome, RuntimeInvocationLedgerError> {
        match self {
            Self::Database(ledger) => Ok(ledger.prepare(identity, fingerprint, decision).await?),
            Self::InMemory(ledger) => Ok(ledger.lock().await.prepare(
                identity.clone(),
                fingerprint.clone(),
                decision.clone(),
            )?),
        }
    }

    pub(crate) async fn dispatch(
        &self,
        identity: &ToolInvocationIdentity,
        owner_id: &str,
    ) -> Result<ToolInvocationRecord, RuntimeInvocationLedgerError> {
        match self {
            Self::Database(ledger) => Ok(ledger
                .claim_dispatch(
                    identity,
                    owner_id,
                    duration_millis(DISPATCH_LEASE_DURATION)?,
                )
                .await?),
            Self::InMemory(ledger) => {
                let lease = lease_from_now(owner_id, DISPATCH_LEASE_DURATION)?;
                Ok(ledger.lock().await.claim_dispatch(identity, lease)?)
            }
        }
    }

    pub(crate) async fn renew_dispatch(
        &self,
        identity: &ToolInvocationIdentity,
        owner_id: &str,
    ) -> Result<ToolInvocationRecord, RuntimeInvocationLedgerError> {
        match self {
            Self::Database(ledger) => Ok(ledger
                .renew_dispatch(
                    identity,
                    owner_id,
                    duration_millis(DISPATCH_LEASE_DURATION)?,
                )
                .await?),
            Self::InMemory(ledger) => {
                let lease = lease_from_now(owner_id, DISPATCH_LEASE_DURATION)?;
                Ok(ledger.lock().await.renew_dispatch(identity, lease)?)
            }
        }
    }

    pub(crate) async fn reconcile_expired_dispatch(
        &self,
        identity: &ToolInvocationIdentity,
    ) -> Result<ToolInvocationRecord, RuntimeInvocationLedgerError> {
        match self {
            Self::Database(ledger) => Ok(ledger.reconcile_expired_dispatch(identity).await?),
            Self::InMemory(ledger) => Ok(ledger
                .lock()
                .await
                .reconcile_expired_dispatch(identity, now_epoch_ms()?)?),
        }
    }

    pub(crate) fn start_lease_heartbeat(
        &self,
        identity: ToolInvocationIdentity,
        owner_id: String,
    ) -> DispatchLeaseHeartbeat {
        let ledger = self.clone();
        let cancel = CancellationToken::new();
        let heartbeat_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(DISPATCH_LEASE_RENEW_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // `interval` ticks immediately once; the initial claim already
            // owns a full lease, so wait for the first renewal interval.
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = heartbeat_cancel.cancelled() => break,
                    _ = interval.tick() => {
                        if let Err(error) = ledger.renew_dispatch(&identity, &owner_id).await {
                            tracing::warn!(
                                user_id = %identity.user_id,
                                session_id = %identity.session_id,
                                run_id = %identity.run_id,
                                turn_chain_id = %identity.turn_chain_id,
                                invocation_id = %identity.invocation_id,
                                dispatch_owner = %owner_id,
                                %error,
                                "tool invocation lease renewal failed"
                            );
                        }
                    }
                }
            }
        });
        DispatchLeaseHeartbeat {
            cancel,
            task: Some(task),
        }
    }

    pub(crate) async fn get(
        &self,
        identity: &ToolInvocationIdentity,
    ) -> Result<Option<ToolInvocationRecord>, RuntimeInvocationLedgerError> {
        match self {
            Self::Database(ledger) => Ok(ledger.get(identity).await?),
            Self::InMemory(ledger) => Ok(ledger.lock().await.get(identity).cloned()),
        }
    }

    pub(crate) async fn complete(
        &self,
        identity: &ToolInvocationIdentity,
        owner_id: &str,
        outcome: &ToolInvocationTerminalOutcome,
    ) -> Result<ToolInvocationRecord, RuntimeInvocationLedgerError> {
        match self {
            Self::Database(ledger) => Ok(ledger
                .compare_and_complete(
                    identity,
                    ToolInvocationState::Dispatched,
                    Some(owner_id),
                    outcome,
                )
                .await?),
            Self::InMemory(ledger) => Ok(ledger.lock().await.compare_and_complete(
                identity,
                ToolInvocationState::Dispatched,
                Some(owner_id),
                outcome.clone(),
            )?),
        }
    }

    async fn reconcile_complete(
        &self,
        identity: &ToolInvocationIdentity,
        outcome: &ToolInvocationTerminalOutcome,
    ) -> Result<ToolInvocationRecord, RuntimeInvocationLedgerError> {
        match self {
            Self::Database(ledger) => Ok(ledger
                .compare_and_complete(identity, ToolInvocationState::OutcomeUnknown, None, outcome)
                .await?),
            Self::InMemory(ledger) => Ok(ledger.lock().await.compare_and_complete(
                identity,
                ToolInvocationState::OutcomeUnknown,
                None,
                outcome.clone(),
            )?),
        }
    }

    pub(crate) async fn mark_outcome_unknown(
        &self,
        identity: &ToolInvocationIdentity,
        owner_id: &str,
    ) -> Result<ToolInvocationRecord, RuntimeInvocationLedgerError> {
        match self {
            Self::Database(ledger) => Ok(ledger.mark_outcome_unknown(identity, owner_id).await?),
            Self::InMemory(ledger) => Ok(ledger
                .lock()
                .await
                .mark_outcome_unknown(identity, owner_id)?),
        }
    }

    /// Establish the authoritative logical invocation and freeze its original
    /// decision without crossing a provider route boundary. This is the only
    /// state in which semantic observation reuse may compete with dispatch.
    pub(crate) async fn prepare_for_execution(
        &self,
        identity: &ToolInvocationIdentity,
        fingerprint: &ToolInvocationFingerprint,
        decision: &ToolInvocationDecision,
        validate_decision: impl FnOnce(&ToolInvocationDecision) -> Result<(), String>,
    ) -> Result<InvocationPrepareDisposition, RuntimeInvocationLedgerError> {
        let record = match self.prepare(identity, fingerprint, decision).await? {
            ToolInvocationPrepareOutcome::Prepared(record)
            | ToolInvocationPrepareOutcome::Existing(record) => record,
        };
        match record.state {
            ToolInvocationState::Prepared => {
                validate_decision(&record.decision)
                    .map_err(RuntimeInvocationLedgerError::InvalidDecision)?;
                Ok(InvocationPrepareDisposition::Prepared {
                    decision: record.decision,
                })
            }
            ToolInvocationState::Dispatched => {
                let authoritative = self.reconcile_expired_dispatch(identity).await?;
                let disposition =
                    disposition_for_existing_record(authoritative)?.ok_or_else(|| {
                        RuntimeInvocationLedgerError::InvalidRecord(
                            "reconciled invocation remained prepared".to_string(),
                        )
                    })?;
                Ok(match disposition {
                    InvocationBeginDisposition::Return(result) => {
                        InvocationPrepareDisposition::Return(result)
                    }
                    InvocationBeginDisposition::Execute { .. } => {
                        return Err(RuntimeInvocationLedgerError::InvalidRecord(
                            "existing dispatched invocation became executable without prepare"
                                .to_string(),
                        ));
                    }
                })
            }
            _ => {
                let disposition = disposition_for_existing_record(record)?.ok_or_else(|| {
                    RuntimeInvocationLedgerError::InvalidRecord(
                        "existing prepared invocation was not dispatchable".to_string(),
                    )
                })?;
                Ok(match disposition {
                    InvocationBeginDisposition::Return(result) => {
                        InvocationPrepareDisposition::Return(result)
                    }
                    InvocationBeginDisposition::Execute { .. } => {
                        return Err(RuntimeInvocationLedgerError::InvalidRecord(
                            "terminal invocation became executable".to_string(),
                        ));
                    }
                })
            }
        }
    }

    /// Atomically claim a previously prepared invocation for provider
    /// dispatch. A concurrent cache completion or dispatch is projected from
    /// the authoritative row instead of being sent again.
    pub(crate) async fn dispatch_prepared(
        &self,
        identity: &ToolInvocationIdentity,
    ) -> Result<InvocationBeginDisposition, RuntimeInvocationLedgerError> {
        let owner_id = uuid::Uuid::now_v7().to_string();
        match self.dispatch(identity, &owner_id).await {
            Ok(record) => Ok(InvocationBeginDisposition::Execute {
                decision: record.decision,
                owner_id,
            }),
            Err(dispatch_error) => {
                let Some(authoritative) = self.get(identity).await? else {
                    return Err(dispatch_error);
                };
                disposition_for_existing_record(authoritative)?.ok_or(dispatch_error)
            }
        }
    }

    /// Atomically complete a still-prepared logical invocation from a trusted
    /// successful observation. `Ok(None)` means a failed CAS was proven to
    /// have left the invocation prepared, so normal dispatch remains safe.
    pub(crate) async fn complete_from_semantic_read_cache(
        &self,
        identity: &ToolInvocationIdentity,
        expected_key: &SemanticReadCacheKey,
        observation: &SemanticReadObservation,
    ) -> Result<Option<astra_tools::ToolResult>, RuntimeInvocationLedgerError> {
        expected_key
            .validate()
            .map_err(|error| RuntimeInvocationLedgerError::InvalidRecord(error.to_string()))?;
        observation
            .validate()
            .map_err(|error| RuntimeInvocationLedgerError::InvalidRecord(error.to_string()))?;
        if observation.key != *expected_key {
            return Err(RuntimeInvocationLedgerError::InvalidRecord(
                "semantic observation key does not match the currently resolved freshness key"
                    .to_string(),
            ));
        }
        let prepared = self.get(identity).await?.ok_or_else(|| {
            RuntimeInvocationLedgerError::InvalidRecord(
                "semantic cache completion has no prepared invocation".to_string(),
            )
        })?;
        if observation.key.tool != prepared.fingerprint.tool
            || observation.key.canonical_arguments_hash
                != prepared.fingerprint.canonical_arguments_hash
            || observation.key.policy_decision_id != prepared.decision.decision_id
        {
            return Err(RuntimeInvocationLedgerError::InvalidRecord(
                "semantic observation identity does not match the prepared tool, arguments, and frozen decision"
                    .to_string(),
            ));
        }
        let completion_source = ToolInvocationCompletionSource::semantic_read_cache(
            observation.key.key_id.clone(),
            observation.observation_id.clone(),
        )
        .map_err(|error| RuntimeInvocationLedgerError::InvalidRecord(error.to_string()))?;
        let completed = match self {
            Self::Database(ledger) => ledger
                .complete_from_semantic_read_cache(
                    identity,
                    &observation.result,
                    &completion_source,
                )
                .await
                .map_err(RuntimeInvocationLedgerError::from),
            Self::InMemory(ledger) => ledger
                .lock()
                .await
                .complete_from_semantic_read_cache(
                    identity,
                    observation.result.clone(),
                    completion_source,
                )
                .map_err(RuntimeInvocationLedgerError::from),
        };
        match completed {
            Ok(record) => Ok(Some(project_terminal_record(&record, false)?)),
            Err(completion_error) => {
                let Some(authoritative) = self.get(identity).await? else {
                    return Err(completion_error);
                };
                if authoritative.state == ToolInvocationState::Prepared {
                    return Ok(None);
                }
                disposition_for_existing_record(authoritative)?.map_or_else(
                    || Err(completion_error),
                    |disposition| match disposition {
                        InvocationBeginDisposition::Return(result) => Ok(Some(result)),
                        InvocationBeginDisposition::Execute { .. } => {
                            Err(RuntimeInvocationLedgerError::InvalidRecord(
                                "cache completion race returned an executable disposition"
                                    .to_string(),
                            ))
                        }
                    },
                )
            }
        }
    }

    pub(crate) async fn confirms_dispatched_outcome(
        &self,
        identity: &ToolInvocationIdentity,
        expected: &ToolInvocationTerminalOutcome,
    ) -> Result<bool, RuntimeInvocationLedgerError> {
        Ok(self.get(identity).await?.is_some_and(|record| {
            record.dispatch_certainty == DispatchCertainty::Dispatched
                && record.outcome.as_ref() == Some(expected)
        }))
    }

    /// Prepare and atomically claim the route boundary. A terminal row is
    /// replayed; an acknowledged or uncertain in-flight row is never sent a
    /// second time.
    #[cfg(test)]
    pub(crate) async fn begin(
        &self,
        identity: &ToolInvocationIdentity,
        fingerprint: &ToolInvocationFingerprint,
        decision: &ToolInvocationDecision,
        validate_decision: impl FnOnce(&ToolInvocationDecision) -> Result<(), String>,
    ) -> Result<InvocationBeginDisposition, RuntimeInvocationLedgerError> {
        match self
            .prepare_for_execution(identity, fingerprint, decision, validate_decision)
            .await?
        {
            InvocationPrepareDisposition::Prepared { .. } => self.dispatch_prepared(identity).await,
            InvocationPrepareDisposition::Return(result) => {
                Ok(InvocationBeginDisposition::Return(result))
            }
        }
    }

    /// Persist the execution result before exposing it. Ambiguous transport
    /// results become `OutcomeUnknown`; a persistence failure after execution
    /// is surfaced as a durability error instead of leaking an undurable
    /// success to the caller.
    pub(crate) async fn finish(
        &self,
        identity: &ToolInvocationIdentity,
        owner_id: &str,
        mut result: astra_tools::ToolResult,
    ) -> astra_tools::ToolResult {
        if result_side_effects_maybe(&result) {
            return match self.mark_outcome_unknown(identity, owner_id).await {
                Ok(_) => {
                    annotate_durable_state(&mut result, ToolInvocationState::OutcomeUnknown, false);
                    let metadata = result.metadata.get_or_insert_with(Map::new);
                    metadata.insert("retryable".to_string(), Value::Bool(false));
                    metadata.insert("resumable".to_string(), Value::Bool(true));
                    result
                }
                Err(error) => {
                    if matches!(self.get(identity).await, Ok(Some(record)) if record.state == ToolInvocationState::OutcomeUnknown)
                    {
                        annotate_durable_state(
                            &mut result,
                            ToolInvocationState::OutcomeUnknown,
                            false,
                        );
                        let metadata = result.metadata.get_or_insert_with(Map::new);
                        metadata.insert("retryable".to_string(), Value::Bool(false));
                        metadata.insert("resumable".to_string(), Value::Bool(true));
                        result
                    } else {
                        durability_error_result(
                            identity,
                            ToolInvocationState::Dispatched,
                            format!("persist outcome-unknown state: {error}"),
                        )
                    }
                }
            };
        }

        let outcome = terminal_outcome_from_result(&result);
        match self.complete(identity, owner_id, &outcome).await {
            Ok(record) => project_terminal_outcome(&outcome, record.state, false),
            Err(complete_error) => {
                if let Ok(Some(record)) = self.get(identity).await {
                    if record.state == ToolInvocationState::OutcomeUnknown {
                        return match self.reconcile_complete(identity, &outcome).await {
                            Ok(reconciled) => {
                                project_terminal_outcome(&outcome, reconciled.state, false)
                            }
                            Err(reconcile_error) => durability_error_result(
                                identity,
                                ToolInvocationState::OutcomeUnknown,
                                format!(
                                    "persist terminal outcome: {complete_error}; reconcile acknowledged outcome: {reconcile_error}"
                                ),
                            ),
                        };
                    }
                    if record.state.is_terminal() {
                        return replay_terminal_record(&record).unwrap_or_else(|error| {
                            durability_error_result(identity, record.state, error.to_string())
                        });
                    }
                }
                match self.mark_outcome_unknown(identity, owner_id).await {
                    Ok(_) => durability_error_result(
                        identity,
                        ToolInvocationState::OutcomeUnknown,
                        format!("persist terminal outcome: {complete_error}"),
                    ),
                    Err(unknown_error) => durability_error_result(
                        identity,
                        ToolInvocationState::Dispatched,
                        format!(
                            "persist terminal outcome: {complete_error}; mark outcome unknown: {unknown_error}"
                        ),
                    ),
                }
            }
        }
    }
}

fn now_epoch_ms() -> Result<u64, RuntimeInvocationLedgerError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| RuntimeInvocationLedgerError::Clock(error.to_string()))?
        .as_millis();
    u64::try_from(millis)
        .map_err(|_| RuntimeInvocationLedgerError::Clock("epoch milliseconds overflow".to_string()))
}

fn duration_millis(duration: Duration) -> Result<u64, RuntimeInvocationLedgerError> {
    u64::try_from(duration.as_millis()).map_err(|_| {
        RuntimeInvocationLedgerError::Clock("dispatch lease duration overflow".to_string())
    })
}

fn lease_from_now(
    owner_id: &str,
    duration: Duration,
) -> Result<ToolInvocationDispatchLease, RuntimeInvocationLedgerError> {
    let expires_at_epoch_ms = now_epoch_ms()?
        .checked_add(duration_millis(duration)?)
        .ok_or_else(|| {
            RuntimeInvocationLedgerError::Clock("dispatch lease deadline overflow".to_string())
        })?;
    ToolInvocationDispatchLease::new(owner_id, expires_at_epoch_ms)
        .map_err(|error| RuntimeInvocationLedgerError::InvalidRecord(error.to_string()))
}

fn disposition_for_existing_record(
    record: ToolInvocationRecord,
) -> Result<Option<InvocationBeginDisposition>, RuntimeInvocationLedgerError> {
    match record.state {
        ToolInvocationState::Prepared => Ok(None),
        ToolInvocationState::Dispatched => Ok(Some(InvocationBeginDisposition::Return(
            pending_result(&record),
        ))),
        ToolInvocationState::OutcomeUnknown => Ok(Some(InvocationBeginDisposition::Return(
            outcome_unknown_result(&record.identity),
        ))),
        ToolInvocationState::Succeeded
        | ToolInvocationState::Failed
        | ToolInvocationState::Rejected => Ok(Some(InvocationBeginDisposition::Return(
            replay_terminal_record(&record)?,
        ))),
    }
}

pub(crate) fn terminal_outcome_from_result(
    result: &astra_tools::ToolResult,
) -> ToolInvocationTerminalOutcome {
    let payload = ToolInvocationResultPayload::bounded_projection(
        result.output.clone(),
        result
            .metadata
            .clone()
            .unwrap_or_default()
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
        result.exit_semantics.and_then(|semantics| {
            serde_json::to_value(semantics)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
        }),
    );
    if !result.is_error {
        return ToolInvocationTerminalOutcome::Succeeded { result: payload };
    }
    if let Some((rejection_code, retryable)) = rejection_evidence(result) {
        return ToolInvocationTerminalOutcome::Rejected {
            result: payload,
            rejection_code,
            retryable,
        };
    }
    ToolInvocationTerminalOutcome::Failed {
        result: payload,
        error_kind: structured_field(result, "error_kind")
            .and_then(|value| value.as_str().map(str::to_string)),
        retryable: structured_field(result, "retryable")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
    }
}

fn rejection_evidence(result: &astra_tools::ToolResult) -> Option<(Option<String>, bool)> {
    if let Some(provider_rejection) = result
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("providerRejection"))
        .and_then(Value::as_object)
    {
        return Some((
            provider_rejection
                .get("code")
                .and_then(Value::as_str)
                .map(str::to_string),
            provider_rejection
                .get("retryable")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ));
    }

    let rejection_code = structured_field(result, "rejection_code")
        .and_then(|value| value.as_str().map(str::to_string));
    let error_kind =
        structured_field(result, "error_kind").and_then(|value| value.as_str().map(str::to_string));
    let reason_kind = structured_field(result, "reason_kind")
        .and_then(|value| value.as_str().map(str::to_string));
    let capability_denial = structured_field(result, "capability_denial").is_some();
    let explicitly_rejected = rejection_code.is_some()
        || capability_denial
        || matches!(
            error_kind.as_deref(),
            Some("approval_denied" | "approval_timeout" | "capability_denied" | "policy_denied")
        )
        || matches!(
            reason_kind.as_deref(),
            Some("policy_denied" | "runtime_surface_denied")
        );
    explicitly_rejected.then(|| {
        (
            rejection_code.or(error_kind),
            structured_field(result, "retryable")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
        )
    })
}

fn structured_field(result: &astra_tools::ToolResult, key: &str) -> Option<Value> {
    result
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get(key))
        .cloned()
        .or_else(|| {
            serde_json::from_str::<Value>(&result.output)
                .ok()
                .and_then(|value| value.get(key).cloned())
        })
}

fn result_side_effects_maybe(result: &astra_tools::ToolResult) -> bool {
    structured_field(result, "side_effects_maybe")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn replay_terminal_record(
    record: &ToolInvocationRecord,
) -> Result<astra_tools::ToolResult, RuntimeInvocationLedgerError> {
    project_terminal_record(record, true)
}

fn project_terminal_record(
    record: &ToolInvocationRecord,
    replay: bool,
) -> Result<astra_tools::ToolResult, RuntimeInvocationLedgerError> {
    let outcome = record.outcome.as_ref().ok_or_else(|| {
        RuntimeInvocationLedgerError::InvalidRecord(format!(
            "terminal invocation {:?} has no typed outcome",
            record.identity
        ))
    })?;
    let mut result = project_terminal_outcome(outcome, record.state, replay);
    if let Some(ToolInvocationCompletionSource::SemanticReadCache {
        cache_key_id,
        observation_id,
        ..
    }) = record.completion_source.as_ref()
    {
        let metadata = result.metadata.get_or_insert_with(Map::new);
        metadata.insert(
            "semantic_read_cache_state".to_string(),
            Value::String("hit".to_string()),
        );
        metadata.insert(
            "semantic_read_cache_key_id".to_string(),
            Value::String(cache_key_id.clone()),
        );
        metadata.insert(
            "semantic_read_observation_id".to_string(),
            Value::String(observation_id.clone()),
        );
    }
    Ok(result)
}

fn project_terminal_outcome(
    outcome: &ToolInvocationTerminalOutcome,
    state: ToolInvocationState,
    replay: bool,
) -> astra_tools::ToolResult {
    let payload = outcome.result();
    let mut result = astra_tools::ToolResult {
        output: payload.output.clone(),
        metadata: (!payload.metadata.is_empty()).then(|| {
            payload
                .metadata
                .clone()
                .into_iter()
                .collect::<Map<String, Value>>()
        }),
        is_error: !matches!(outcome, ToolInvocationTerminalOutcome::Succeeded { .. }),
        exit_semantics: payload
            .exit_semantics
            .as_ref()
            .and_then(|semantics| serde_json::from_value(Value::String(semantics.clone())).ok()),
    };
    let metadata = result.metadata.get_or_insert_with(Map::new);
    match outcome {
        ToolInvocationTerminalOutcome::Succeeded { .. } => {}
        ToolInvocationTerminalOutcome::Failed {
            error_kind,
            retryable,
            ..
        } => {
            if let Some(error_kind) = error_kind {
                metadata
                    .entry("error_kind".to_string())
                    .or_insert_with(|| Value::String(error_kind.clone()));
            }
            metadata
                .entry("retryable".to_string())
                .or_insert(Value::Bool(*retryable));
        }
        ToolInvocationTerminalOutcome::Rejected {
            rejection_code,
            retryable,
            ..
        } => {
            if let Some(rejection_code) = rejection_code {
                metadata
                    .entry("rejection_code".to_string())
                    .or_insert_with(|| Value::String(rejection_code.clone()));
            }
            metadata
                .entry("retryable".to_string())
                .or_insert(Value::Bool(*retryable));
        }
    }
    annotate_durable_state(&mut result, state, replay);
    result
}

fn annotate_durable_state(
    result: &mut astra_tools::ToolResult,
    state: ToolInvocationState,
    replay: bool,
) {
    let metadata = result.metadata.get_or_insert_with(Map::new);
    metadata.insert(
        "durable_invocation_state".to_string(),
        Value::String(state_label(state).to_string()),
    );
    metadata.insert("invocation_replay".to_string(), Value::Bool(replay));
}

fn pending_result(record: &ToolInvocationRecord) -> astra_tools::ToolResult {
    let mut result = invocation_state_result(
        &record.identity,
        ToolInvocationState::Dispatched,
        "tool_invocation_in_progress",
        false,
        "The same logical tool invocation is already dispatched; Astra will not send it again.",
    );
    if let Some(lease) = record.dispatch_lease.as_ref() {
        let metadata = result.metadata.get_or_insert_with(Map::new);
        metadata.insert(
            "dispatch_lease_expires_at_epoch_ms".to_string(),
            Value::from(lease.expires_at_epoch_ms),
        );
    }
    result
}

fn outcome_unknown_result(identity: &ToolInvocationIdentity) -> astra_tools::ToolResult {
    invocation_state_result(
        identity,
        ToolInvocationState::OutcomeUnknown,
        "tool_invocation_outcome_unknown",
        true,
        "The provider may have applied this logical tool invocation, but no acknowledged outcome is durable; Astra will not retry it automatically.",
    )
}

fn durability_error_result(
    identity: &ToolInvocationIdentity,
    state: ToolInvocationState,
    detail: String,
) -> astra_tools::ToolResult {
    invocation_state_result(
        identity,
        state,
        "tool_invocation_durability",
        true,
        &format!(
            "Tool execution crossed the dispatch boundary but its durable outcome could not be committed: {detail}"
        ),
    )
}

pub(crate) fn ledger_unavailable_result(
    identity: &ToolInvocationIdentity,
    detail: impl std::fmt::Display,
) -> astra_tools::ToolResult {
    let mut result = astra_tools::ToolResult::error(format!(
        "The logical tool invocation could not enter its durable dispatch ledger: {detail}"
    ));
    result.metadata = Some(Map::from_iter([
        (
            "error_kind".to_string(),
            Value::String("tool_invocation_ledger".to_string()),
        ),
        ("invocation_identity".to_string(), json!(identity)),
        ("side_effects_maybe".to_string(), Value::Bool(false)),
        ("retryable".to_string(), Value::Bool(true)),
        ("resumable".to_string(), Value::Bool(true)),
    ]));
    result
}

fn invocation_state_result(
    identity: &ToolInvocationIdentity,
    state: ToolInvocationState,
    error_kind: &str,
    side_effects_maybe: bool,
    message: &str,
) -> astra_tools::ToolResult {
    let mut result = astra_tools::ToolResult::error(message.to_string());
    result.metadata = Some(Map::from_iter([
        (
            "error_kind".to_string(),
            Value::String(error_kind.to_string()),
        ),
        (
            "durable_invocation_state".to_string(),
            Value::String(state_label(state).to_string()),
        ),
        ("invocation_identity".to_string(), json!(identity)),
        (
            "side_effects_maybe".to_string(),
            Value::Bool(side_effects_maybe),
        ),
        ("retryable".to_string(), Value::Bool(false)),
        ("resumable".to_string(), Value::Bool(true)),
    ]));
    result
}

fn state_label(state: ToolInvocationState) -> &'static str {
    match state {
        ToolInvocationState::Prepared => "prepared",
        ToolInvocationState::Dispatched => "dispatched",
        ToolInvocationState::Succeeded => "succeeded",
        ToolInvocationState::Failed => "failed",
        ToolInvocationState::Rejected => "rejected",
        ToolInvocationState::OutcomeUnknown => "outcome_unknown",
    }
}

#[derive(Debug, Error)]
pub(crate) enum RuntimeInvocationLedgerError {
    #[error(transparent)]
    Database(Box<astra_services::tool_invocation_ledger::ToolInvocationLedgerStoreError>),
    #[error(transparent)]
    InMemory(Box<InvocationLedgerError>),
    #[error("invalid durable invocation record: {0}")]
    InvalidRecord(String),
    #[error("invalid frozen tool invocation decision: {0}")]
    InvalidDecision(String),
    #[error("tool invocation dispatch clock error: {0}")]
    Clock(String),
}

impl From<astra_services::tool_invocation_ledger::ToolInvocationLedgerStoreError>
    for RuntimeInvocationLedgerError
{
    fn from(error: astra_services::tool_invocation_ledger::ToolInvocationLedgerStoreError) -> Self {
        Self::Database(Box::new(error))
    }
}

impl From<InvocationLedgerError> for RuntimeInvocationLedgerError {
    fn from(error: InvocationLedgerError) -> Self {
        Self::InMemory(Box::new(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_tools::exit_semantics::ExitSemantics;
    use astra_turn_types::{
        DurableToolReference, SemanticFreshnessFact, SemanticFreshnessScope, SemanticReadCacheKey,
        SemanticReadFreshnessContext, SemanticReadObservation,
    };
    use serde_json::json;

    fn identity(invocation_id: &str) -> ToolInvocationIdentity {
        ToolInvocationIdentity::new("user", "session", "run", "turn", invocation_id).unwrap()
    }

    fn fingerprint(arguments: &Value) -> ToolInvocationFingerprint {
        let decision = decision("decision-v1");
        fingerprint_for(arguments, &decision)
    }

    fn fingerprint_for(
        arguments: &Value,
        decision: &ToolInvocationDecision,
    ) -> ToolInvocationFingerprint {
        ToolInvocationFingerprint::new(
            DurableToolReference::built_in("bash", "contract-v1").unwrap(),
            arguments,
            &decision.decision_id,
        )
        .unwrap()
    }

    fn decision(label: &str) -> ToolInvocationDecision {
        ToolInvocationDecision::new(&json!({"decision": label})).unwrap()
    }

    fn semantic_observation(
        arguments: &Value,
        decision: &ToolInvocationDecision,
        output: &str,
    ) -> SemanticReadObservation {
        semantic_observation_at_revision(arguments, decision, output, "revision-1")
    }

    fn semantic_observation_at_revision(
        arguments: &Value,
        decision: &ToolInvocationDecision,
        output: &str,
        revision: &str,
    ) -> SemanticReadObservation {
        let freshness = SemanticReadFreshnessContext::new(
            "user:session",
            vec![
                SemanticFreshnessFact::new(
                    SemanticFreshnessScope::Resource,
                    "resource-a",
                    revision,
                )
                .unwrap(),
            ],
        )
        .unwrap();
        let key = SemanticReadCacheKey::new(
            DurableToolReference::built_in("bash", "contract-v1").unwrap(),
            arguments,
            &decision.decision_id,
            &freshness,
        )
        .unwrap();
        SemanticReadObservation::from_terminal_outcome(
            key,
            &ToolInvocationTerminalOutcome::Succeeded {
                result: ToolInvocationResultPayload {
                    output: output.to_string(),
                    metadata: BTreeMap::new(),
                    exit_semantics: None,
                },
            },
        )
        .unwrap()
    }

    async fn begin(
        ledger: &RuntimeToolInvocationLedger,
        identity: &ToolInvocationIdentity,
        fingerprint: &ToolInvocationFingerprint,
    ) -> Result<InvocationBeginDisposition, RuntimeInvocationLedgerError> {
        ledger
            .begin(identity, fingerprint, &decision("decision-v1"), |_| Ok(()))
            .await
    }

    fn execute_owner(disposition: InvocationBeginDisposition) -> String {
        match disposition {
            InvocationBeginDisposition::Execute { owner_id, .. } => owner_id,
            InvocationBeginDisposition::Return(_) => panic!("invocation should execute"),
        }
    }

    #[tokio::test]
    async fn logical_identity_not_semantic_arguments_controls_replay() {
        let ledger = RuntimeToolInvocationLedger::new(None);
        let args = json!({"command": "deploy"});
        let first = identity("call-1");
        let second = identity("call-2");
        let fingerprint = fingerprint(&args);

        let first_owner = execute_owner(begin(&ledger, &first, &fingerprint).await.unwrap());
        let first_result = astra_tools::ToolResult::text("deployed".to_string());
        let committed = ledger.finish(&first, &first_owner, first_result).await;
        assert!(!committed.is_error);
        assert_eq!(
            committed.metadata.as_ref().unwrap()["durable_invocation_state"],
            "succeeded"
        );

        let replay = match begin(&ledger, &first, &fingerprint).await.unwrap() {
            InvocationBeginDisposition::Return(result) => result,
            InvocationBeginDisposition::Execute { .. } => panic!("terminal identity must replay"),
        };
        assert_eq!(replay.output, "deployed");
        assert_eq!(replay.metadata.as_ref().unwrap()["invocation_replay"], true);

        assert!(matches!(
            begin(&ledger, &second, &fingerprint).await.unwrap(),
            InvocationBeginDisposition::Execute { .. }
        ));
    }

    #[tokio::test]
    async fn same_identity_with_changed_arguments_fails_instead_of_replaying() {
        let ledger = RuntimeToolInvocationLedger::new(None);
        let identity = identity("call-1");
        begin(
            &ledger,
            &identity,
            &fingerprint(&json!({"command": "deploy"})),
        )
        .await
        .unwrap();

        assert!(matches!(
            begin(&ledger, &identity, &fingerprint(&json!({"command": "destroy"}))).await,
            Err(RuntimeInvocationLedgerError::InMemory(error))
                if matches!(*error, InvocationLedgerError::IdentityConflict { .. })
        ));
    }

    #[tokio::test]
    async fn active_dispatch_returns_observable_pending_without_a_second_claim() {
        let ledger = RuntimeToolInvocationLedger::new(None);
        let identity = identity("call-active");
        let fingerprint = fingerprint(&json!({"command": "deploy"}));
        let owner = execute_owner(begin(&ledger, &identity, &fingerprint).await.unwrap());

        let pending = match begin(&ledger, &identity, &fingerprint).await.unwrap() {
            InvocationBeginDisposition::Return(result) => result,
            InvocationBeginDisposition::Execute { .. } => {
                panic!("a live lease must fence duplicate dispatch")
            }
        };
        let metadata = pending.metadata.as_ref().unwrap();
        assert_eq!(metadata["durable_invocation_state"], "dispatched");
        assert_eq!(metadata["error_kind"], "tool_invocation_in_progress");
        assert!(metadata["dispatch_lease_expires_at_epoch_ms"].is_u64());
        let record = ledger.get(&identity).await.unwrap().unwrap();
        assert_eq!(record.attempt_count, 1);
        assert_eq!(record.dispatch_lease.unwrap().owner_id, owner);
    }

    #[tokio::test]
    async fn acknowledged_late_result_reconciles_an_expired_dispatch() {
        let ledger = RuntimeToolInvocationLedger::new(None);
        let identity = identity("call-late-result");
        let fingerprint = fingerprint(&json!({"command": "deploy"}));
        let owner = execute_owner(begin(&ledger, &identity, &fingerprint).await.unwrap());
        match &ledger {
            RuntimeToolInvocationLedger::InMemory(inner) => {
                let reconciled = inner
                    .lock()
                    .await
                    .reconcile_expired_dispatch(&identity, u64::MAX)
                    .unwrap();
                assert_eq!(reconciled.state, ToolInvocationState::OutcomeUnknown);
            }
            RuntimeToolInvocationLedger::Database(_) => unreachable!("test ledger is in-memory"),
        }

        let completed = ledger
            .finish(
                &identity,
                &owner,
                astra_tools::ToolResult::text("deployed".to_string()),
            )
            .await;
        assert!(!completed.is_error, "{completed:?}");
        assert_eq!(
            completed.metadata.as_ref().unwrap()["durable_invocation_state"],
            "succeeded"
        );
        let replay = match begin(&ledger, &identity, &fingerprint).await.unwrap() {
            InvocationBeginDisposition::Return(result) => result,
            InvocationBeginDisposition::Execute { .. } => panic!("terminal result must replay"),
        };
        assert_eq!(replay.output, "deployed");
        assert_eq!(replay.metadata.as_ref().unwrap()["invocation_replay"], true);
    }

    #[tokio::test]
    async fn prepared_resume_uses_original_decision_when_live_policy_changed() {
        let ledger = RuntimeToolInvocationLedger::new(None);
        let identity = identity("call-1");
        let args = json!({"command": "deploy"});
        let original = decision("original-policy");
        ledger
            .prepare(&identity, &fingerprint_for(&args, &original), &original)
            .await
            .unwrap();
        let changed = decision("changed-live-policy");

        let resumed = ledger
            .begin(
                &identity,
                &fingerprint_for(&args, &changed),
                &changed,
                |_| Ok(()),
            )
            .await
            .unwrap();
        match resumed {
            InvocationBeginDisposition::Execute { decision, .. } => {
                assert_eq!(decision, original);
            }
            InvocationBeginDisposition::Return(_) => panic!("prepared invocation should resume"),
        }
    }

    #[tokio::test]
    async fn cache_hit_completes_prepared_without_dispatch_and_replays_provenance() {
        let ledger = RuntimeToolInvocationLedger::new(None);
        let identity = identity("call-cache");
        let arguments = json!({"command": "read"});
        let decision = decision("decision-v1");
        let fingerprint = fingerprint_for(&arguments, &decision);
        assert!(matches!(
            ledger
                .prepare_for_execution(&identity, &fingerprint, &decision, |_| Ok(()))
                .await
                .unwrap(),
            InvocationPrepareDisposition::Prepared { .. }
        ));
        let observation = semantic_observation(&arguments, &decision, "cached result");

        let completed = ledger
            .complete_from_semantic_read_cache(&identity, &observation.key, &observation)
            .await
            .unwrap()
            .expect("cache completion should return the terminal result");
        assert_eq!(completed.output, "cached result");
        let metadata = completed.metadata.as_ref().unwrap();
        assert_eq!(metadata["durable_invocation_state"], "succeeded");
        assert_eq!(metadata["invocation_replay"], false);
        assert_eq!(metadata["semantic_read_cache_state"], "hit");
        assert_eq!(
            metadata["semantic_read_observation_id"],
            observation.observation_id
        );
        let record = ledger.get(&identity).await.unwrap().unwrap();
        assert_eq!(record.dispatch_certainty, DispatchCertainty::NotDispatched);
        assert_eq!(record.attempt_count, 0);
        assert!(record.dispatch_lease.is_none());

        let replay = match ledger.dispatch_prepared(&identity).await.unwrap() {
            InvocationBeginDisposition::Return(result) => result,
            InvocationBeginDisposition::Execute { .. } => {
                panic!("cache-completed invocation must not dispatch")
            }
        };
        assert_eq!(replay.output, "cached result");
        assert_eq!(replay.metadata.as_ref().unwrap()["invocation_replay"], true);
        assert_eq!(
            replay.metadata.as_ref().unwrap()["semantic_read_cache_state"],
            "hit"
        );
    }

    #[tokio::test]
    async fn cache_completion_rejects_observation_for_different_arguments() {
        let ledger = RuntimeToolInvocationLedger::new(None);
        let identity = identity("call-cache-mismatch");
        let arguments = json!({"command": "read-a"});
        let decision = decision("decision-v1");
        ledger
            .prepare_for_execution(
                &identity,
                &fingerprint_for(&arguments, &decision),
                &decision,
                |_| Ok(()),
            )
            .await
            .unwrap();
        let wrong = semantic_observation(&json!({"command": "read-b"}), &decision, "wrong");
        let expected_key = semantic_observation(&arguments, &decision, "expected").key;

        assert!(matches!(
            ledger
                .complete_from_semantic_read_cache(&identity, &expected_key, &wrong)
                .await,
            Err(RuntimeInvocationLedgerError::InvalidRecord(message))
                if message.contains("does not match")
        ));
        assert_eq!(
            ledger.get(&identity).await.unwrap().unwrap().state,
            ToolInvocationState::Prepared
        );
        assert!(matches!(
            ledger.dispatch_prepared(&identity).await.unwrap(),
            InvocationBeginDisposition::Execute { .. }
        ));
    }

    #[tokio::test]
    async fn cache_completion_rejects_observation_for_previous_freshness_revision() {
        let ledger = RuntimeToolInvocationLedger::new(None);
        let identity = identity("call-cache-stale-revision");
        let arguments = json!({"command": "read"});
        let decision = decision("decision-v1");
        ledger
            .prepare_for_execution(
                &identity,
                &fingerprint_for(&arguments, &decision),
                &decision,
                |_| Ok(()),
            )
            .await
            .unwrap();
        let stale = semantic_observation_at_revision(
            &arguments,
            &decision,
            "stale",
            "revision-before-resume",
        );
        let current = semantic_observation_at_revision(
            &arguments,
            &decision,
            "current",
            "revision-after-resume",
        );

        assert!(matches!(
            ledger
                .complete_from_semantic_read_cache(&identity, &current.key, &stale)
                .await,
            Err(RuntimeInvocationLedgerError::InvalidRecord(message))
                if message.contains("currently resolved freshness key")
        ));
        assert_eq!(
            ledger.get(&identity).await.unwrap().unwrap().state,
            ToolInvocationState::Prepared
        );
        assert!(matches!(
            ledger.dispatch_prepared(&identity).await.unwrap(),
            InvocationBeginDisposition::Execute { .. }
        ));
    }

    #[tokio::test]
    async fn dispatch_claim_wins_cache_completion_race_without_overwriting_provider_state() {
        let ledger = RuntimeToolInvocationLedger::new(None);
        let identity = identity("call-cache-race");
        let arguments = json!({"command": "read"});
        let decision = decision("decision-v1");
        ledger
            .prepare_for_execution(
                &identity,
                &fingerprint_for(&arguments, &decision),
                &decision,
                |_| Ok(()),
            )
            .await
            .unwrap();
        assert!(matches!(
            ledger.dispatch_prepared(&identity).await.unwrap(),
            InvocationBeginDisposition::Execute { .. }
        ));
        let observation = semantic_observation(&arguments, &decision, "cached result");

        let pending = ledger
            .complete_from_semantic_read_cache(&identity, &observation.key, &observation)
            .await
            .unwrap()
            .expect("dispatch winner should project its authoritative state");
        assert_eq!(
            pending.metadata.as_ref().unwrap()["error_kind"],
            "tool_invocation_in_progress"
        );
        let record = ledger.get(&identity).await.unwrap().unwrap();
        assert_eq!(record.state, ToolInvocationState::Dispatched);
        assert_eq!(record.attempt_count, 1);
        assert!(record.completion_source.is_none());
    }

    #[test]
    fn terminal_classification_requires_structured_rejection_evidence() {
        let prose_only = astra_tools::ToolResult::error("request denied".to_string());
        assert!(matches!(
            terminal_outcome_from_result(&prose_only),
            ToolInvocationTerminalOutcome::Failed { .. }
        ));

        let mut approval = astra_tools::ToolResult::error("request denied".to_string());
        approval.metadata = Some(Map::from_iter([
            (
                "rejection_code".to_string(),
                Value::String("approval_denied".to_string()),
            ),
            ("retryable".to_string(), Value::Bool(false)),
        ]));
        assert!(matches!(
            terminal_outcome_from_result(&approval),
            ToolInvocationTerminalOutcome::Rejected {
                rejection_code: Some(code),
                retryable: false,
                ..
            } if code == "approval_denied"
        ));

        let mut provider = astra_tools::ToolResult::error("provider rejected".to_string());
        provider.metadata = Some(Map::from_iter([(
            "providerRejection".to_string(),
            json!({"code": "tenant_policy", "retryable": true}),
        )]));
        assert!(matches!(
            terminal_outcome_from_result(&provider),
            ToolInvocationTerminalOutcome::Rejected {
                rejection_code: Some(code),
                retryable: true,
                ..
            } if code == "tenant_policy"
        ));
    }

    #[tokio::test]
    async fn oversized_acknowledged_result_completes_with_explicit_bounded_projection() {
        let ledger = RuntimeToolInvocationLedger::new(None);
        let identity = identity("call-large-result");
        let fingerprint = fingerprint(&json!({"command": "verbose"}));
        let owner = execute_owner(begin(&ledger, &identity, &fingerprint).await.unwrap());
        let raw = astra_tools::ToolResult::text(
            "界".repeat(astra_turn_types::TOOL_INVOCATION_RESULT_OUTPUT_MAX_BYTES),
        );

        let projected = ledger.finish(&identity, &owner, raw).await;
        assert!(!projected.is_error, "{projected:?}");
        assert!(
            projected
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("astraResultProjection"))
                .is_some()
        );
        let record = ledger.get(&identity).await.unwrap().unwrap();
        assert_eq!(record.state, ToolInvocationState::Succeeded);
        record.outcome.as_ref().unwrap().validate().unwrap();
    }

    #[tokio::test]
    async fn ambiguous_execution_becomes_outcome_unknown_and_is_not_redispatched() {
        let ledger = RuntimeToolInvocationLedger::new(None);
        let identity = identity("call-1");
        let fingerprint = fingerprint(&json!({"command": "deploy"}));
        let owner = execute_owner(begin(&ledger, &identity, &fingerprint).await.unwrap());
        let mut ambiguous = astra_tools::ToolResult::error("connection lost".to_string());
        ambiguous.metadata = Some(Map::from_iter([
            ("execution_started".to_string(), Value::Bool(true)),
            ("side_effects_maybe".to_string(), Value::Bool(true)),
        ]));

        let result = ledger.finish(&identity, &owner, ambiguous).await;
        assert_eq!(
            result.metadata.as_ref().unwrap()["durable_invocation_state"],
            "outcome_unknown"
        );
        assert_eq!(result.metadata.as_ref().unwrap()["retryable"], false);
        let resumed = match begin(&ledger, &identity, &fingerprint).await.unwrap() {
            InvocationBeginDisposition::Return(result) => result,
            InvocationBeginDisposition::Execute { .. } => {
                panic!("uncertain invocation must not retry")
            }
        };
        let metadata = resumed.metadata.unwrap();
        assert_eq!(metadata["error_kind"], "tool_invocation_outcome_unknown");
        assert_eq!(metadata["side_effects_maybe"], true);
        assert_eq!(metadata["retryable"], false);
    }

    #[tokio::test]
    async fn replay_preserves_payload_and_exit_semantics() {
        let ledger = RuntimeToolInvocationLedger::new(None);
        let identity = identity("call-1");
        let fingerprint = fingerprint(&json!({"command": "test -f missing"}));
        let owner = execute_owner(begin(&ledger, &identity, &fingerprint).await.unwrap());
        let result = astra_tools::ToolResult {
            output: "not found".to_string(),
            metadata: Some(Map::from_iter([("exit_code".to_string(), json!(1))])),
            is_error: false,
            exit_semantics: Some(ExitSemantics::DomainNegative),
        };
        ledger.finish(&identity, &owner, result).await;

        let replay = match begin(&ledger, &identity, &fingerprint).await.unwrap() {
            InvocationBeginDisposition::Return(result) => result,
            InvocationBeginDisposition::Execute { .. } => panic!("terminal identity must replay"),
        };
        assert_eq!(replay.output, "not found");
        assert_eq!(replay.exit_semantics, Some(ExitSemantics::DomainNegative));
        assert_eq!(replay.metadata.as_ref().unwrap()["exit_code"], 1);
    }
}
