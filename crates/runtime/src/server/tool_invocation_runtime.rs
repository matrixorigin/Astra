//! Runtime adapter over the shared durable invocation ledger contract.
//!
//! This module is also the single projection boundary between ephemeral
//! `ToolResult` values and durable typed outcomes. Classification only uses
//! machine-readable fields; provider/user errors are never inferred from
//! human-readable prose.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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
    Prepared {
        decision: ToolInvocationDecision,
    },
    Return(astra_tools::ToolResult),
    Superseded {
        result: astra_tools::ToolResult,
        user_intent_event_index: i64,
    },
}

/// Per-call durable control boundary supplied by the agentic loop. The owner
/// pod capability is bound separately when the database ledger is composed,
/// so provider-authored arguments cannot choose either authority value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DurableDispatchAdmission {
    pub expected_control_epoch: i64,
    pub expected_owner_generation: u64,
}

pub(crate) const DISPATCH_LEASE_DURATION: Duration = Duration::from_secs(90);
pub(crate) const DISPATCH_LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(30);
const PROCESS_LOCAL_LEDGER_RETENTION_LIMIT: usize = 10_000;
const PROCESS_LOCAL_LEDGER_PRUNE_BUDGET: usize = 16;

#[derive(Default)]
pub(crate) struct ProcessLocalInvocationLedgers {
    runs: Mutex<HashMap<(String, String), Arc<tokio::sync::Mutex<InMemoryInvocationLedger>>>>,
    terminal_runs: Mutex<TerminalRunQueue>,
    empty_runs: Mutex<TerminalRunQueue>,
    entry_count: AtomicUsize,
}

#[derive(Default)]
struct TerminalRunQueue {
    queued: HashSet<(String, String)>,
    order: VecDeque<(String, String)>,
}

impl ProcessLocalInvocationLedgers {
    fn run_ledger(
        &self,
        identity: &ToolInvocationIdentity,
    ) -> Arc<tokio::sync::Mutex<InMemoryInvocationLedger>> {
        self.run_ledger_for(&identity.user_id, &identity.run_id)
    }

    fn run_ledger_for(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Arc<tokio::sync::Mutex<InMemoryInvocationLedger>> {
        let key = (user_id.to_string(), run_id.to_string());
        let mut runs = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            runs.entry(key)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(Default::default()))),
        )
    }

    fn existing_run_ledger(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Option<Arc<tokio::sync::Mutex<InMemoryInvocationLedger>>> {
        self.runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&(user_id.to_string(), run_id.to_string()))
            .cloned()
    }

    fn len(&self) -> usize {
        self.entry_count.load(Ordering::Relaxed)
    }

    fn try_reserve_entry(&self) -> bool {
        self.entry_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < PROCESS_LOCAL_LEDGER_RETENTION_LIMIT).then_some(current + 1)
            })
            .is_ok()
    }

    fn release_entry_reservation(&self) {
        self.entry_count.fetch_sub(1, Ordering::AcqRel);
    }

    fn note_terminal(&self, identity: &ToolInvocationIdentity) {
        self.defer_terminal_run((identity.user_id.clone(), identity.run_id.clone()));
    }

    fn defer_terminal_run(&self, key: (String, String)) {
        let mut queue = self
            .terminal_runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if queue.queued.insert(key.clone()) {
            queue.order.push_back(key);
        }
    }

    fn take_terminal_run(&self) -> Option<(String, String)> {
        let mut queue = self
            .terminal_runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = queue.order.pop_front()?;
        queue.queued.remove(&key);
        Some(key)
    }

    async fn remove_run(&self, user_id: &str, run_id: &str) -> usize {
        let Some(run_ledger) = self.existing_run_ledger(user_id, run_id) else {
            return 0;
        };
        let removed = run_ledger.lock().await.remove_run(user_id, run_id);
        self.entry_count.fetch_sub(removed, Ordering::Relaxed);

        // Retire the empty per-run lock only when no operation can still
        // publish into it. This keeps recreated executors on one authority
        // shard and prevents an ABA split between old and new ledgers.
        let key = (user_id.to_string(), run_id.to_string());
        let mut runs = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if Arc::strong_count(&run_ledger) == 2
            && runs
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(current, &run_ledger))
        {
            runs.remove(&key);
        } else {
            drop(runs);
            let mut empty = self
                .empty_runs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if empty.queued.insert(key.clone()) {
                empty.order.push_back(key);
            }
        }
        removed
    }

    fn cleanup_one_empty_run(&self) {
        let key = {
            let mut empty = self
                .empty_runs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(key) = empty.order.pop_front() else {
                return;
            };
            empty.queued.remove(&key);
            key
        };
        let mut runs = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let removable = runs
            .get(&key)
            .is_some_and(|ledger| Arc::strong_count(ledger) == 1);
        if removable {
            runs.remove(&key);
            return;
        }
        drop(runs);
        let mut empty = self
            .empty_runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if empty.queued.insert(key.clone()) {
            empty.order.push_back(key);
        }
    }
}

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
    Database {
        ledger: astra_services::tool_invocation_ledger::DatabaseToolInvocationLedger,
        expected_owner_pod_id: Option<Arc<str>>,
    },
    InMemory {
        ledger: Arc<ProcessLocalInvocationLedgers>,
        /// Exact process-local run authority shared by every executor created
        /// by one lifecycle. `None` is retained for isolated ledger unit tests
        /// that never present durable dispatch admission.
        run_engine: Option<crate::server::run::engine::RunEngine>,
        #[cfg(test)]
        dispatch_publish_barrier: Option<Arc<ProcessLocalDispatchPublishBarrier>>,
    },
}

#[cfg(test)]
pub(crate) struct ProcessLocalDispatchPublishBarrier {
    action_admitted: tokio::sync::Barrier,
    allow_publish: tokio::sync::Barrier,
}

impl RuntimeToolInvocationLedger {
    async fn prune_retired_process_local_runs(&self) {
        let Self::InMemory {
            ledger,
            run_engine: Some(run_engine),
            ..
        } = self
        else {
            return;
        };
        if ledger.len() < PROCESS_LOCAL_LEDGER_RETENTION_LIMIT {
            return;
        }
        for _ in 0..PROCESS_LOCAL_LEDGER_PRUNE_BUDGET {
            let Some((candidate_user_id, candidate_run_id)) = ledger.take_terminal_run() else {
                break;
            };
            let candidate_key = (candidate_user_id.clone(), candidate_run_id.clone());
            let Some(run_ledger) =
                ledger.existing_run_ledger(&candidate_user_id, &candidate_run_id)
            else {
                continue;
            };
            let Some(action_fence) =
                run_engine.process_local_action_fence(&candidate_user_id, &candidate_run_id)
            else {
                ledger.defer_terminal_run(candidate_key);
                continue;
            };
            let Ok(_action_guard) = action_fence.try_lock_owned() else {
                // Retention is opportunistic and must never put an unrelated
                // user's prepare behind a busy run's action lifecycle.
                ledger.defer_terminal_run(candidate_key);
                continue;
            };
            let retired = match run_engine
                .load_run(&candidate_user_id, &candidate_run_id)
                .await
            {
                Ok(None) => true,
                Ok(Some(run)) => astra_services::runs::durable_run_status_is_terminal(&run.status),
                Err(error) => {
                    tracing::warn!(
                        user_id = %candidate_user_id,
                        run_id = %candidate_run_id,
                        %error,
                        "process-local invocation retention could not verify run retirement"
                    );
                    false
                }
            };
            let has_unsettled_dispatch = run_ledger.lock().await.has_unsettled_dispatch();
            if retired && !has_unsettled_dispatch {
                // Serialize retirement with every prepare/dispatch for this
                // run, then clear only its indexed member set. A dispatched
                // or outcome-unknown call survives terminal run cleanup so a
                // late provider acknowledgement can still reconcile.
                ledger
                    .remove_run(&candidate_user_id, &candidate_run_id)
                    .await;
            } else {
                ledger.defer_terminal_run(candidate_key);
            }
            if ledger.len() < PROCESS_LOCAL_LEDGER_RETENTION_LIMIT {
                break;
            }
        }
    }

    pub(crate) fn new(pool: Option<astra_core::SharedPool>) -> Self {
        Self::new_with_owner(pool, None)
    }

    pub(crate) fn new_with_owner(
        pool: Option<astra_core::SharedPool>,
        expected_owner_pod_id: Option<String>,
    ) -> Self {
        match pool {
            Some(pool) => Self::Database {
                ledger: astra_services::tool_invocation_ledger::DatabaseToolInvocationLedger::new(
                    pool,
                ),
                expected_owner_pod_id: expected_owner_pod_id.map(Arc::from),
            },
            None => Self::InMemory {
                ledger: Arc::new(ProcessLocalInvocationLedgers::default()),
                run_engine: None,
                #[cfg(test)]
                dispatch_publish_barrier: None,
            },
        }
    }

    pub(crate) fn new_process_local(
        run_engine: crate::server::run::engine::RunEngine,
    ) -> Result<Self, String> {
        if run_engine.uses_transactional_invocation_admission() {
            return Err(
                "database run authority cannot be bound to an in-memory invocation ledger"
                    .to_string(),
            );
        }
        Ok(Self::InMemory {
            ledger: Arc::new(ProcessLocalInvocationLedgers::default()),
            run_engine: Some(run_engine),
            #[cfg(test)]
            dispatch_publish_barrier: None,
        })
    }

    #[cfg(test)]
    fn with_process_local_dispatch_publish_barrier(
        mut self,
        barrier: Arc<ProcessLocalDispatchPublishBarrier>,
    ) -> Self {
        if let Self::InMemory {
            dispatch_publish_barrier,
            ..
        } = &mut self
        {
            *dispatch_publish_barrier = Some(barrier);
        }
        self
    }

    pub(crate) fn new_database(
        pool: astra_core::SharedPool,
        expected_owner_pod_id: String,
    ) -> Self {
        Self::Database {
            ledger: astra_services::tool_invocation_ledger::DatabaseToolInvocationLedger::new(pool),
            expected_owner_pod_id: Some(Arc::from(expected_owner_pod_id)),
        }
    }

    pub(crate) async fn prepare(
        &self,
        identity: &ToolInvocationIdentity,
        fingerprint: &ToolInvocationFingerprint,
        decision: &ToolInvocationDecision,
    ) -> Result<ToolInvocationPrepareOutcome, RuntimeInvocationLedgerError> {
        if let Self::InMemory { ledger, .. } = self {
            ledger.cleanup_one_empty_run();
        }
        self.prune_retired_process_local_runs().await;
        match self {
            Self::Database { ledger, .. } => {
                Ok(ledger.prepare(identity, fingerprint, decision).await?)
            }
            Self::InMemory {
                ledger, run_engine, ..
            } => {
                let _action_guard = if let Some(run_engine) = run_engine {
                    let action_fence = run_engine
                        .process_local_action_fence(&identity.user_id, &identity.run_id)
                        .ok_or(RuntimeInvocationLedgerError::UnsupportedAtomicAdmission)?;
                    let guard = action_fence.lock_owned().await;
                    let run = run_engine
                        .load_run(&identity.user_id, &identity.run_id)
                        .await
                        .map_err(RuntimeInvocationLedgerError::ProcessLocalActionAdmission)?
                        .ok_or(RuntimeInvocationLedgerError::ProcessLocalRunMissing)?;
                    if run.session_id != identity.session_id {
                        return Err(RuntimeInvocationLedgerError::ProcessLocalRunMissing);
                    }
                    if run.status != astra_core::STATUS_RUNNING {
                        if astra_services::runs::durable_run_status_is_terminal(&run.status) {
                            ledger.defer_terminal_run((
                                identity.user_id.clone(),
                                identity.run_id.clone(),
                            ));
                        }
                        return Err(RuntimeInvocationLedgerError::ProcessLocalActionInactive {
                            status: run.status,
                        });
                    }
                    Some(guard)
                } else {
                    None
                };
                let run_ledger = ledger.run_ledger(identity);
                let mut run_ledger = run_ledger.lock().await;
                let is_new = run_ledger.get(identity).is_none();
                if is_new && !ledger.try_reserve_entry() {
                    return Err(RuntimeInvocationLedgerError::ProcessLocalCapacity {
                        limit: PROCESS_LOCAL_LEDGER_RETENTION_LIMIT,
                    });
                }
                let outcome =
                    run_ledger.prepare(identity.clone(), fingerprint.clone(), decision.clone());
                if outcome.is_err() && is_new {
                    ledger.release_entry_reservation();
                }
                let outcome = outcome?;
                // Every run stays in the bounded retirement rotation. This
                // covers Prepared calls whose task disappears before dispatch
                // and therefore never emits a terminal invocation transition.
                ledger.defer_terminal_run((identity.user_id.clone(), identity.run_id.clone()));
                Ok(outcome)
            }
        }
    }

    pub(crate) async fn dispatch(
        &self,
        identity: &ToolInvocationIdentity,
        owner_id: &str,
        admission: Option<DurableDispatchAdmission>,
    ) -> Result<ToolInvocationRecord, RuntimeInvocationLedgerError> {
        match self {
            Self::Database {
                ledger,
                expected_owner_pod_id,
            } => {
                let admission = admission
                    .ok_or(RuntimeInvocationLedgerError::MissingDurableDispatchAdmission)?;
                let expected_owner_pod_id = expected_owner_pod_id
                    .as_deref()
                    .ok_or(RuntimeInvocationLedgerError::MissingDurableOwnerCapability)?;
                Ok(ledger
                    .claim_dispatch(
                        identity,
                        owner_id,
                        duration_millis(DISPATCH_LEASE_DURATION)?,
                        astra_services::tool_invocation_ledger::ToolInvocationDispatchAdmission {
                            expected_control_epoch: admission.expected_control_epoch,
                            expected_owner_generation: admission.expected_owner_generation,
                            expected_owner_pod_id: expected_owner_pod_id.to_string(),
                        },
                    )
                    .await?)
            }
            Self::InMemory {
                ledger,
                run_engine,
                #[cfg(test)]
                dispatch_publish_barrier,
            } => {
                if admission.is_none() && run_engine.is_some() {
                    return Err(RuntimeInvocationLedgerError::MissingDurableDispatchAdmission);
                }
                if let Some(admission) = admission {
                    let run_engine = run_engine
                        .as_ref()
                        .ok_or(RuntimeInvocationLedgerError::UnsupportedAtomicAdmission)?;
                    let action_fence = run_engine
                        .process_local_action_fence(&identity.user_id, &identity.run_id)
                        .ok_or(RuntimeInvocationLedgerError::UnsupportedAtomicAdmission)?;
                    // Fixed lock order: action fence -> invocation ledger ->
                    // run store. Every process-local run mutation takes the
                    // same outer fence before its run lock, so pause/cancel or
                    // guidance cannot commit between grant and claim.
                    let _action_guard = action_fence.lock_owned().await;
                    let lease = lease_from_now(owner_id, DISPATCH_LEASE_DURATION)?;
                    let run_ledger = ledger.run_ledger(identity);
                    let mut authoritative_ledger = run_ledger.lock().await;
                    let candidate =
                        authoritative_ledger.dispatch_claim_candidate(identity, lease)?;
                    let action_id = format!("tool_invocation:{}", identity.storage_key());
                    let outcome = run_engine
                        .begin_action_while_process_local_fence_held(
                            &identity.user_id,
                            &identity.run_id,
                            crate::turn::run_control::ActionAdmissionRequest {
                                action_id,
                                expected_session_id: identity.session_id.clone(),
                                expected_control_epoch: admission.expected_control_epoch,
                                expected_owner_generation: Some(
                                    admission.expected_owner_generation,
                                ),
                            },
                        )
                        .await
                        .map_err(RuntimeInvocationLedgerError::ProcessLocalActionAdmission)?;
                    match outcome {
                        astra_services::runs::AtomicRunActionAdmission::Started { .. } => {
                            #[cfg(test)]
                            if let Some(barrier) = dispatch_publish_barrier {
                                barrier.action_admitted.wait().await;
                                barrier.allow_publish.wait().await;
                            }
                            // Publish the already-validated candidate only
                            // after the grant is durable in the same critical
                            // section. No fallible operation follows.
                            return Ok(authoritative_ledger.commit_dispatch_claim(candidate)?);
                        }
                        astra_services::runs::AtomicRunActionAdmission::AlreadyStarted { event_index }
                        | astra_services::runs::AtomicRunActionAdmission::AckRecoveredStarted { event_index } => {
                            return Err(
                                RuntimeInvocationLedgerError::ProcessLocalActionAlreadyStarted {
                                    event_index,
                                },
                            );
                        }
                        astra_services::runs::AtomicRunActionAdmission::Superseded {
                            user_intent_event_index,
                        } => {
                            return Err(RuntimeInvocationLedgerError::ProcessLocalActionSuperseded {
                                user_intent_event_index,
                            });
                        }
                        astra_services::runs::AtomicRunActionAdmission::Inactive { status } => {
                            if astra_services::runs::durable_run_status_is_terminal(&status) {
                                ledger.defer_terminal_run((
                                    identity.user_id.clone(),
                                    identity.run_id.clone(),
                                ));
                            }
                            return Err(RuntimeInvocationLedgerError::ProcessLocalActionInactive {
                                status,
                            });
                        }
                        astra_services::runs::AtomicRunActionAdmission::OwnerGenerationMismatch {
                            actual_owner_generation,
                        } => {
                            return Err(
                                RuntimeInvocationLedgerError::ProcessLocalOwnerGenerationMismatch {
                                    actual_owner_generation,
                                },
                            );
                        }
                        astra_services::runs::AtomicRunActionAdmission::Missing => {
                            ledger.defer_terminal_run((
                                identity.user_id.clone(),
                                identity.run_id.clone(),
                            ));
                            return Err(RuntimeInvocationLedgerError::ProcessLocalRunMissing);
                        }
                    }
                }
                let lease = lease_from_now(owner_id, DISPATCH_LEASE_DURATION)?;
                Ok(ledger
                    .run_ledger(identity)
                    .lock()
                    .await
                    .claim_dispatch(identity, lease)?)
            }
        }
    }

    pub(crate) async fn renew_dispatch(
        &self,
        identity: &ToolInvocationIdentity,
        owner_id: &str,
    ) -> Result<ToolInvocationRecord, RuntimeInvocationLedgerError> {
        match self {
            Self::Database { ledger, .. } => Ok(ledger
                .renew_dispatch(
                    identity,
                    owner_id,
                    duration_millis(DISPATCH_LEASE_DURATION)?,
                )
                .await?),
            Self::InMemory { ledger, .. } => {
                let lease = lease_from_now(owner_id, DISPATCH_LEASE_DURATION)?;
                Ok(ledger
                    .run_ledger(identity)
                    .lock()
                    .await
                    .renew_dispatch(identity, lease)?)
            }
        }
    }

    pub(crate) async fn reconcile_expired_dispatch(
        &self,
        identity: &ToolInvocationIdentity,
    ) -> Result<ToolInvocationRecord, RuntimeInvocationLedgerError> {
        match self {
            Self::Database { ledger, .. } => {
                Ok(ledger.reconcile_expired_dispatch(identity).await?)
            }
            Self::InMemory { ledger, .. } => {
                let record = ledger
                    .run_ledger(identity)
                    .lock()
                    .await
                    .reconcile_expired_dispatch(identity, now_epoch_ms()?)?;
                if record.state.is_terminal() {
                    ledger.note_terminal(identity);
                }
                Ok(record)
            }
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
            Self::Database { ledger, .. } => Ok(ledger.get(identity).await?),
            Self::InMemory { ledger, .. } => {
                let Some(run_ledger) =
                    ledger.existing_run_ledger(&identity.user_id, &identity.run_id)
                else {
                    return Ok(None);
                };
                Ok(run_ledger.lock().await.get(identity).cloned())
            }
        }
    }

    pub(crate) async fn lifecycle_diagnostics(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: Option<&str>,
    ) -> Result<
        Option<astra_services::tool_invocation_ledger::ToolInvocationLifecycleDiagnostics>,
        RuntimeInvocationLedgerError,
    > {
        match self {
            Self::Database { ledger, .. } => Ok(Some(
                ledger
                    .lifecycle_diagnostics(user_id, session_id, run_id)
                    .await?,
            )),
            Self::InMemory { .. } => Ok(None),
        }
    }

    pub(crate) async fn complete(
        &self,
        identity: &ToolInvocationIdentity,
        owner_id: &str,
        outcome: &ToolInvocationTerminalOutcome,
    ) -> Result<ToolInvocationRecord, RuntimeInvocationLedgerError> {
        match self {
            Self::Database { ledger, .. } => Ok(ledger
                .compare_and_complete(
                    identity,
                    ToolInvocationState::Dispatched,
                    Some(owner_id),
                    outcome,
                )
                .await?),
            Self::InMemory { ledger, .. } => {
                let record = ledger
                    .run_ledger(identity)
                    .lock()
                    .await
                    .compare_and_complete(
                        identity,
                        ToolInvocationState::Dispatched,
                        Some(owner_id),
                        outcome.clone(),
                    )?;
                ledger.note_terminal(identity);
                Ok(record)
            }
        }
    }

    async fn reconcile_complete(
        &self,
        identity: &ToolInvocationIdentity,
        outcome: &ToolInvocationTerminalOutcome,
    ) -> Result<ToolInvocationRecord, RuntimeInvocationLedgerError> {
        match self {
            Self::Database { ledger, .. } => Ok(ledger
                .compare_and_complete(identity, ToolInvocationState::OutcomeUnknown, None, outcome)
                .await?),
            Self::InMemory { ledger, .. } => {
                let record = ledger
                    .run_ledger(identity)
                    .lock()
                    .await
                    .compare_and_complete(
                        identity,
                        ToolInvocationState::OutcomeUnknown,
                        None,
                        outcome.clone(),
                    )?;
                ledger.note_terminal(identity);
                Ok(record)
            }
        }
    }

    pub(crate) async fn mark_outcome_unknown(
        &self,
        identity: &ToolInvocationIdentity,
        owner_id: &str,
    ) -> Result<ToolInvocationRecord, RuntimeInvocationLedgerError> {
        match self {
            Self::Database { ledger, .. } => {
                Ok(ledger.mark_outcome_unknown(identity, owner_id).await?)
            }
            Self::InMemory { ledger, .. } => {
                let record = ledger
                    .run_ledger(identity)
                    .lock()
                    .await
                    .mark_outcome_unknown(identity, owner_id)?;
                ledger.note_terminal(identity);
                Ok(record)
            }
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
                let superseding_event_index = guidance_completion_event_index(&authoritative);
                let disposition =
                    disposition_for_existing_record(authoritative)?.ok_or_else(|| {
                        RuntimeInvocationLedgerError::InvalidRecord(
                            "reconciled invocation remained prepared".to_string(),
                        )
                    })?;
                Ok(match disposition {
                    InvocationBeginDisposition::Return(result) => match superseding_event_index {
                        Some(user_intent_event_index) => InvocationPrepareDisposition::Superseded {
                            result,
                            user_intent_event_index,
                        },
                        None => InvocationPrepareDisposition::Return(result),
                    },
                    InvocationBeginDisposition::Execute { .. } => {
                        return Err(RuntimeInvocationLedgerError::InvalidRecord(
                            "existing dispatched invocation became executable without prepare"
                                .to_string(),
                        ));
                    }
                })
            }
            _ => {
                let superseding_event_index = guidance_completion_event_index(&record);
                let disposition = disposition_for_existing_record(record)?.ok_or_else(|| {
                    RuntimeInvocationLedgerError::InvalidRecord(
                        "existing prepared invocation was not dispatchable".to_string(),
                    )
                })?;
                Ok(match disposition {
                    InvocationBeginDisposition::Return(result) => match superseding_event_index {
                        Some(user_intent_event_index) => InvocationPrepareDisposition::Superseded {
                            result,
                            user_intent_event_index,
                        },
                        None => InvocationPrepareDisposition::Return(result),
                    },
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
    #[cfg(test)]
    pub(crate) async fn dispatch_prepared(
        &self,
        identity: &ToolInvocationIdentity,
    ) -> Result<InvocationBeginDisposition, RuntimeInvocationLedgerError> {
        self.dispatch_prepared_with_admission(identity, None).await
    }

    pub(crate) async fn dispatch_prepared_with_admission(
        &self,
        identity: &ToolInvocationIdentity,
        admission: Option<DurableDispatchAdmission>,
    ) -> Result<InvocationBeginDisposition, RuntimeInvocationLedgerError> {
        let owner_id = uuid::Uuid::now_v7().to_string();
        match self.dispatch(identity, &owner_id, admission).await {
            Ok(record) => Ok(InvocationBeginDisposition::Execute {
                decision: record.decision,
                owner_id,
            }),
            Err(dispatch_error) => {
                if dispatch_error.is_action_authority_failure() {
                    return Err(dispatch_error);
                }
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
            Self::Database { ledger, .. } => ledger
                .complete_from_semantic_read_cache(
                    identity,
                    &observation.result,
                    &completion_source,
                )
                .await
                .map_err(RuntimeInvocationLedgerError::from),
            Self::InMemory { ledger, .. } => ledger
                .run_ledger(identity)
                .lock()
                .await
                .complete_from_semantic_read_cache(
                    identity,
                    observation.result.clone(),
                    completion_source,
                )
                .map(|record| {
                    ledger.note_terminal(identity);
                    record
                })
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
            InvocationPrepareDisposition::Superseded { result, .. } => {
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

fn guidance_completion_event_index(record: &ToolInvocationRecord) -> Option<i64> {
    match record.completion_source.as_ref() {
        Some(ToolInvocationCompletionSource::SupersededByGuidance {
            user_intent_event_index,
            ..
        }) => Some(*user_intent_event_index),
        _ => None,
    }
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
    if let Some(completion_source) = record.completion_source.as_ref() {
        let metadata = result.metadata.get_or_insert_with(Map::new);
        match completion_source {
            ToolInvocationCompletionSource::SemanticReadCache {
                cache_key_id,
                observation_id,
                ..
            } => {
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
            ToolInvocationCompletionSource::RunClosure { run_status, .. } => {
                metadata.insert(
                    "tool_invocation_completion_source".to_string(),
                    Value::String("run_closure".to_string()),
                );
                metadata.insert("run_status".to_string(), Value::String(run_status.clone()));
            }
            ToolInvocationCompletionSource::SupersededByGuidance {
                user_intent_event_index,
                ..
            } => {
                metadata.insert(
                    "tool_invocation_completion_source".to_string(),
                    Value::String("superseded_by_guidance".to_string()),
                );
                metadata.insert(
                    "user_intent_event_index".to_string(),
                    Value::Number((*user_intent_event_index).into()),
                );
            }
        }
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
        true,
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
    #[error("durable tool dispatch is missing its applied control boundary")]
    MissingDurableDispatchAdmission,
    #[error("durable tool dispatch is missing its execution-owner capability")]
    MissingDurableOwnerCapability,
    #[error("an in-memory invocation ledger cannot atomically admit durable run control")]
    UnsupportedAtomicAdmission,
    #[error("process-local invocation action admission failed: {0}")]
    ProcessLocalActionAdmission(String),
    #[error(
        "process-local invocation was superseded by user intent event {user_intent_event_index}"
    )]
    ProcessLocalActionSuperseded { user_intent_event_index: i64 },
    #[error(
        "process-local invocation admission grant {event_index} exists without its atomic ledger claim"
    )]
    ProcessLocalActionAlreadyStarted { event_index: i64 },
    #[error("process-local invocation cannot start while its run is {status}")]
    ProcessLocalActionInactive { status: String },
    #[error(
        "process-local invocation owner generation was superseded by {actual_owner_generation}"
    )]
    ProcessLocalOwnerGenerationMismatch { actual_owner_generation: u64 },
    #[error("process-local invocation run no longer exists")]
    ProcessLocalRunMissing,
    #[error("process-local invocation authority capacity {limit} is exhausted")]
    ProcessLocalCapacity { limit: usize },
}

impl RuntimeInvocationLedgerError {
    pub(crate) fn superseding_user_intent_event_index(&self) -> Option<i64> {
        match self {
            Self::Database(error) => match error.as_ref() {
                astra_services::tool_invocation_ledger::ToolInvocationLedgerStoreError::ActionSuperseded {
                    user_intent_event_index,
                    ..
                } => Some(*user_intent_event_index),
                _ => None,
            },
            Self::ProcessLocalActionSuperseded {
                user_intent_event_index,
            } => Some(*user_intent_event_index),
            _ => None,
        }
    }

    pub(crate) fn is_action_authority_failure(&self) -> bool {
        match self {
            Self::MissingDurableDispatchAdmission
            | Self::MissingDurableOwnerCapability
            | Self::UnsupportedAtomicAdmission
            | Self::ProcessLocalActionAdmission(_)
            | Self::ProcessLocalActionSuperseded { .. }
            | Self::ProcessLocalActionAlreadyStarted { .. }
            | Self::ProcessLocalActionInactive { .. }
            | Self::ProcessLocalOwnerGenerationMismatch { .. }
            | Self::ProcessLocalRunMissing
            | Self::ProcessLocalCapacity { .. } => true,
            Self::Database(error) => matches!(
                error.as_ref(),
                astra_services::tool_invocation_ledger::ToolInvocationLedgerStoreError::ActionSuperseded { .. }
                    | astra_services::tool_invocation_ledger::ToolInvocationLedgerStoreError::ActionAlreadyStarted { .. }
                    | astra_services::tool_invocation_ledger::ToolInvocationLedgerStoreError::ActionAdmissionFailed { .. }
                    | astra_services::tool_invocation_ledger::ToolInvocationLedgerStoreError::RunNotFound { .. }
                    | astra_services::tool_invocation_ledger::ToolInvocationLedgerStoreError::RunNotExecutable { .. }
                    | astra_services::tool_invocation_ledger::ToolInvocationLedgerStoreError::RunOwnerGenerationMismatch { .. }
                    | astra_services::tool_invocation_ledger::ToolInvocationLedgerStoreError::RunOwnerMismatch { .. }
            ),
            _ => false,
        }
    }
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

    fn identity_for(run_id: &str, invocation_id: &str) -> ToolInvocationIdentity {
        ToolInvocationIdentity::new("user", "session", run_id, "turn", invocation_id).unwrap()
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
    async fn process_local_ledger_replays_across_executor_recreation() {
        let run_engine = crate::server::run::engine::RunEngine::new(Arc::new(
            astra_services::runs::InMemoryRunStateStore::new(),
        ));
        run_engine
            .start_run("local-replay", "user", "session")
            .await
            .unwrap();
        let ledger = RuntimeToolInvocationLedger::new_process_local(run_engine).unwrap();
        let invocation = identity_for("local-replay", "call-replay");
        let fingerprint = fingerprint(&json!({"command": "deploy"}));
        ledger
            .prepare(&invocation, &fingerprint, &decision("decision-v1"))
            .await
            .unwrap();
        let owner = execute_owner(
            ledger
                .dispatch_prepared_with_admission(
                    &invocation,
                    Some(DurableDispatchAdmission {
                        expected_control_epoch: -1,
                        expected_owner_generation: 0,
                    }),
                )
                .await
                .unwrap(),
        );
        ledger
            .finish(
                &invocation,
                &owner,
                astra_tools::ToolResult::text("deployed".to_string()),
            )
            .await;

        // A recreated root or child executor receives a clone of the
        // lifecycle-owned ledger, not a fresh invocation truth.
        let recreated_executor_ledger = ledger.clone();
        let replay = match recreated_executor_ledger
            .prepare_for_execution(&invocation, &fingerprint, &decision("decision-v1"), |_| {
                Ok(())
            })
            .await
            .unwrap()
        {
            InvocationPrepareDisposition::Return(result) => result,
            InvocationPrepareDisposition::Prepared { .. } => {
                panic!("terminal invocation must not be prepared again")
            }
            InvocationPrepareDisposition::Superseded { .. } => {
                panic!("terminal invocation must not be superseded")
            }
        };
        assert_eq!(replay.output, "deployed");
        assert_eq!(replay.metadata.as_ref().unwrap()["invocation_replay"], true);
        assert_eq!(
            recreated_executor_ledger
                .get(&invocation)
                .await
                .unwrap()
                .unwrap()
                .attempt_count,
            1
        );
    }

    #[tokio::test]
    async fn process_local_retention_never_head_of_line_blocks_an_unrelated_run() {
        let run_engine = crate::server::run::engine::RunEngine::new(Arc::new(
            astra_services::runs::InMemoryRunStateStore::new(),
        ));
        run_engine
            .start_run("retention-busy-a", "user", "session")
            .await
            .unwrap();
        run_engine
            .start_run("retention-live-b", "user", "session-b")
            .await
            .unwrap();
        let ledger = RuntimeToolInvocationLedger::new_process_local(run_engine.clone()).unwrap();
        let busy = identity_for("retention-busy-a", "call-a");
        ledger
            .prepare(
                &busy,
                &fingerprint(&json!({"command": "a"})),
                &decision("decision-v1"),
            )
            .await
            .unwrap();

        let internal = match &ledger {
            RuntimeToolInvocationLedger::InMemory { ledger, .. } => Arc::clone(ledger),
            RuntimeToolInvocationLedger::Database { .. } => unreachable!(),
        };
        internal
            .entry_count
            .store(PROCESS_LOCAL_LEDGER_RETENTION_LIMIT, Ordering::Relaxed);
        internal.defer_terminal_run(("user".to_string(), "retention-busy-a".to_string()));
        let busy_fence = run_engine
            .process_local_action_fence("user", "retention-busy-a")
            .unwrap();
        let _busy_guard = busy_fence.lock_owned().await;

        let live =
            ToolInvocationIdentity::new("user", "session-b", "retention-live-b", "turn", "call-b")
                .unwrap();
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            ledger.prepare(
                &live,
                &fingerprint(&json!({"command": "b"})),
                &decision("decision-v1"),
            ),
        )
        .await
        .expect("busy run retention must not block an unrelated prepare");
        assert!(matches!(
            result,
            Err(RuntimeInvocationLedgerError::ProcessLocalCapacity {
                limit: PROCESS_LOCAL_LEDGER_RETENTION_LIMIT
            })
        ));
    }

    #[tokio::test]
    async fn process_local_capacity_preserves_unsettled_dispatch_authority() {
        let run_engine = crate::server::run::engine::RunEngine::new(Arc::new(
            astra_services::runs::InMemoryRunStateStore::new(),
        ));
        run_engine
            .start_run("capacity-unsettled", "user", "session")
            .await
            .unwrap();
        run_engine
            .start_run("capacity-next", "user", "session-next")
            .await
            .unwrap();
        let ledger = RuntimeToolInvocationLedger::new_process_local(run_engine.clone()).unwrap();
        let unsettled = identity_for("capacity-unsettled", "call-unsettled");
        ledger
            .prepare(
                &unsettled,
                &fingerprint(&json!({"command": "deploy"})),
                &decision("decision-v1"),
            )
            .await
            .unwrap();
        ledger
            .dispatch_prepared_with_admission(
                &unsettled,
                Some(DurableDispatchAdmission {
                    expected_control_epoch: -1,
                    expected_owner_generation: 0,
                }),
            )
            .await
            .unwrap();
        assert!(
            run_engine
                .persist_typed_cancellation_fixture(
                    "user",
                    "session",
                    "capacity-unsettled",
                    &[astra_core::STATUS_RUNNING],
                    astra_turn_core::orchestration_types::CancellationOrigin::Runtime,
                )
                .await
                .unwrap()
        );

        let internal = match &ledger {
            RuntimeToolInvocationLedger::InMemory { ledger, .. } => Arc::clone(ledger),
            RuntimeToolInvocationLedger::Database { .. } => unreachable!(),
        };
        internal
            .entry_count
            .store(PROCESS_LOCAL_LEDGER_RETENTION_LIMIT, Ordering::Relaxed);
        internal.defer_terminal_run(("user".to_string(), "capacity-unsettled".to_string()));
        let next = ToolInvocationIdentity::new(
            "user",
            "session-next",
            "capacity-next",
            "turn",
            "call-next",
        )
        .unwrap();
        let error = ledger
            .prepare(
                &next,
                &fingerprint(&json!({"command": "next"})),
                &decision("decision-v1"),
            )
            .await
            .expect_err("capacity must fail closed while authority is unsettled");
        assert!(matches!(
            error,
            RuntimeInvocationLedgerError::ProcessLocalCapacity { .. }
        ));
        assert_eq!(
            ledger.get(&unsettled).await.unwrap().unwrap().state,
            ToolInvocationState::Dispatched,
            "capacity cleanup must preserve late-result reconciliation authority"
        );
    }

    #[tokio::test]
    async fn process_local_capacity_retires_prepared_call_after_run_cancellation() {
        let run_engine = crate::server::run::engine::RunEngine::new(Arc::new(
            astra_services::runs::InMemoryRunStateStore::new(),
        ));
        run_engine
            .start_run("capacity-prepared", "user", "session")
            .await
            .unwrap();
        run_engine
            .start_run("capacity-replacement", "user", "replacement-session")
            .await
            .unwrap();
        let ledger = RuntimeToolInvocationLedger::new_process_local(run_engine.clone()).unwrap();
        let abandoned = identity_for("capacity-prepared", "call-prepared");
        ledger
            .prepare(
                &abandoned,
                &fingerprint(&json!({"command": "never-dispatched"})),
                &decision("decision-v1"),
            )
            .await
            .unwrap();
        run_engine
            .persist_typed_cancellation_fixture(
                "user",
                "session",
                "capacity-prepared",
                &[astra_core::STATUS_RUNNING],
                astra_turn_core::orchestration_types::CancellationOrigin::Runtime,
            )
            .await
            .unwrap();
        let internal = match &ledger {
            RuntimeToolInvocationLedger::InMemory { ledger, .. } => Arc::clone(ledger),
            RuntimeToolInvocationLedger::Database { .. } => unreachable!(),
        };
        internal
            .entry_count
            .store(PROCESS_LOCAL_LEDGER_RETENTION_LIMIT, Ordering::Relaxed);

        let replacement = ToolInvocationIdentity::new(
            "user",
            "replacement-session",
            "capacity-replacement",
            "turn",
            "call-replacement",
        )
        .unwrap();
        ledger
            .prepare(
                &replacement,
                &fingerprint(&json!({"command": "replacement"})),
                &decision("decision-v1"),
            )
            .await
            .expect("terminal Prepared history must be retired before capacity rejection");
        assert!(ledger.get(&abandoned).await.unwrap().is_none());
        assert!(ledger.get(&replacement).await.unwrap().is_some());
        // Retirement may overlap a transient reader/reference to the empty
        // per-run ledger. The next prepare performs constant-time deferred
        // map cleanup without scanning invocation history.
        ledger
            .prepare(
                &replacement,
                &fingerprint(&json!({"command": "replacement"})),
                &decision("decision-v1"),
            )
            .await
            .unwrap();
        assert!(
            !internal
                .runs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains_key(&("user".to_string(), "capacity-prepared".to_string())),
            "an empty retired per-run ledger must leave the authority map once readers release"
        );
    }

    #[tokio::test]
    async fn process_local_dispatch_and_user_intent_share_one_action_fence() {
        let run_engine = crate::server::run::engine::RunEngine::new(Arc::new(
            astra_services::runs::InMemoryRunStateStore::new(),
        ));
        run_engine
            .start_run("local-intent-race", "user", "session")
            .await
            .unwrap();
        let ledger = RuntimeToolInvocationLedger::new_process_local(run_engine.clone()).unwrap();
        let invocation = identity_for("local-intent-race", "call-race");
        let fingerprint = fingerprint(&json!({"command": "deploy"}));
        ledger
            .prepare(&invocation, &fingerprint, &decision("decision-v1"))
            .await
            .unwrap();

        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let dispatch = {
            let ledger = ledger.clone();
            let invocation = invocation.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                ledger
                    .dispatch_prepared_with_admission(
                        &invocation,
                        Some(DurableDispatchAdmission {
                            expected_control_epoch: -1,
                            expected_owner_generation: 0,
                        }),
                    )
                    .await
            })
        };
        let intent = {
            let run_engine = run_engine.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                run_engine
                    .append_event(
                        "user",
                        "session",
                        "local-intent-race",
                        json!({
                            "event_type": "user_intent",
                            "idempotency_key": "user_intent:intent-race",
                            "data": {"intent_id": "intent-race", "input": {"text": "stop"}}
                        }),
                    )
                    .await
            })
        };
        barrier.wait().await;
        let dispatch = dispatch.await.unwrap();
        intent.await.unwrap().unwrap();

        let record = ledger.get(&invocation).await.unwrap().unwrap();
        match dispatch {
            Ok(InvocationBeginDisposition::Execute { .. }) => {
                assert_eq!(record.attempt_count, 1, "action grant won the fence");
            }
            Err(RuntimeInvocationLedgerError::ProcessLocalActionSuperseded { .. }) => {
                assert_eq!(record.attempt_count, 0, "user intent won the fence");
                assert_eq!(record.state, ToolInvocationState::Prepared);
            }
            Ok(InvocationBeginDisposition::Return(_)) => {
                panic!("fresh prepared invocation cannot already be terminal")
            }
            Err(error) => panic!("unexpected local action race error: {error}"),
        }
    }

    #[tokio::test]
    async fn process_local_cancel_cannot_enter_between_action_grant_and_ledger_claim() {
        let run_engine = crate::server::run::engine::RunEngine::new(Arc::new(
            astra_services::runs::InMemoryRunStateStore::new(),
        ));
        run_engine
            .start_run("local-publish-race", "user", "session")
            .await
            .unwrap();
        let publish_barrier = Arc::new(ProcessLocalDispatchPublishBarrier {
            action_admitted: tokio::sync::Barrier::new(2),
            allow_publish: tokio::sync::Barrier::new(2),
        });
        let ledger = RuntimeToolInvocationLedger::new_process_local(run_engine.clone())
            .unwrap()
            .with_process_local_dispatch_publish_barrier(publish_barrier.clone());
        let invocation = identity_for("local-publish-race", "call-race");
        let fingerprint = fingerprint(&json!({"command": "deploy"}));
        ledger
            .prepare(&invocation, &fingerprint, &decision("decision-v1"))
            .await
            .unwrap();

        let dispatch = {
            let ledger = ledger.clone();
            let invocation = invocation.clone();
            tokio::spawn(async move {
                ledger
                    .dispatch_prepared_with_admission(
                        &invocation,
                        Some(DurableDispatchAdmission {
                            expected_control_epoch: -1,
                            expected_owner_generation: 0,
                        }),
                    )
                    .await
            })
        };
        publish_barrier.action_admitted.wait().await;
        let mut cancel = {
            let run_engine = run_engine.clone();
            tokio::spawn(async move {
                run_engine
                    .persist_typed_cancellation_fixture(
                        "user",
                        "session",
                        "local-publish-race",
                        &[astra_core::STATUS_RUNNING],
                        astra_turn_core::orchestration_types::CancellationOrigin::User,
                    )
                    .await
            })
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut cancel)
                .await
                .is_err(),
            "cancel must wait for the combined action/ledger critical section"
        );
        publish_barrier.allow_publish.wait().await;
        assert!(matches!(
            dispatch.await.unwrap().unwrap(),
            InvocationBeginDisposition::Execute { .. }
        ));
        cancel.await.unwrap().unwrap();

        let record = ledger.get(&invocation).await.unwrap().unwrap();
        assert_eq!(record.state, ToolInvocationState::Dispatched);
        assert_eq!(record.attempt_count, 1);
        let run = run_engine
            .load_run("user", "local-publish-race")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, astra_core::STATUS_CANCELLED);
        assert_eq!(
            run.events
                .iter()
                .filter(|event| {
                    event.get("event_type").and_then(Value::as_str)
                        == Some("action_admission_granted")
                })
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn process_local_dispatch_rejects_wrong_owner_or_session_identity() {
        let run_engine = crate::server::run::engine::RunEngine::new(Arc::new(
            astra_services::runs::InMemoryRunStateStore::new(),
        ));
        run_engine
            .start_run("identity-run", "user", "session")
            .await
            .unwrap();
        let ledger = RuntimeToolInvocationLedger::new_process_local(run_engine).unwrap();
        let internal = match &ledger {
            RuntimeToolInvocationLedger::InMemory { ledger, .. } => Arc::clone(ledger),
            RuntimeToolInvocationLedger::Database { .. } => unreachable!(),
        };
        for invocation in [
            ToolInvocationIdentity::new("user", "wrong-session", "identity-run", "turn", "a")
                .unwrap(),
            ToolInvocationIdentity::new("other-user", "session", "identity-run", "turn", "b")
                .unwrap(),
        ] {
            let fingerprint = fingerprint(&json!({"command": "deploy"}));
            assert!(matches!(
                ledger
                    .prepare(&invocation, &fingerprint, &decision("decision-v1"))
                    .await,
                Err(RuntimeInvocationLedgerError::ProcessLocalRunMissing)
            ));
            assert!(ledger.get(&invocation).await.unwrap().is_none());
        }
        assert_eq!(
            internal.len(),
            0,
            "rejected identities must not consume capacity"
        );
    }

    #[tokio::test]
    async fn process_local_cancel_fences_prepared_invocation_without_dispatch() {
        let run_engine = crate::server::run::engine::RunEngine::new(Arc::new(
            astra_services::runs::InMemoryRunStateStore::new(),
        ));
        run_engine
            .start_run("local-cancel", "user", "session")
            .await
            .unwrap();
        let ledger = RuntimeToolInvocationLedger::new_process_local(run_engine.clone()).unwrap();
        let invocation = identity_for("local-cancel", "call-cancelled");
        let fingerprint = fingerprint(&json!({"command": "deploy"}));
        ledger
            .prepare(&invocation, &fingerprint, &decision("decision-v1"))
            .await
            .unwrap();
        run_engine
            .persist_typed_cancellation_fixture(
                "user",
                "session",
                "local-cancel",
                &[astra_core::STATUS_RUNNING],
                astra_turn_core::orchestration_types::CancellationOrigin::User,
            )
            .await
            .unwrap();

        let result = ledger
            .dispatch_prepared_with_admission(
                &invocation,
                Some(DurableDispatchAdmission {
                    expected_control_epoch: -1,
                    expected_owner_generation: 0,
                }),
            )
            .await;
        assert!(matches!(
            result,
            Err(RuntimeInvocationLedgerError::ProcessLocalActionInactive { .. })
        ));
        let record = ledger.get(&invocation).await.unwrap().unwrap();
        assert_eq!(record.state, ToolInvocationState::Prepared);
        assert_eq!(record.attempt_count, 0);
    }

    #[tokio::test]
    async fn process_local_orphaned_grant_cannot_dispatch_after_cancel() {
        let run_engine = crate::server::run::engine::RunEngine::new(Arc::new(
            astra_services::runs::InMemoryRunStateStore::new(),
        ));
        run_engine
            .start_run("orphaned-grant", "user", "session")
            .await
            .unwrap();
        let ledger = RuntimeToolInvocationLedger::new_process_local(run_engine.clone()).unwrap();
        let invocation = identity_for("orphaned-grant", "call-orphaned");
        let fingerprint = fingerprint(&json!({"command": "deploy"}));
        ledger
            .prepare(&invocation, &fingerprint, &decision("decision-v1"))
            .await
            .unwrap();
        let action_id = format!("tool_invocation:{}", invocation.storage_key());
        assert!(matches!(
            crate::turn::run_control::UserIntentProvider::begin_action(
                &run_engine,
                "user",
                "orphaned-grant",
                crate::turn::run_control::ActionAdmissionRequest {
                    action_id,
                    expected_session_id: "session".to_string(),
                    expected_control_epoch: -1,
                    expected_owner_generation: Some(0),
                },
            )
            .await
            .unwrap(),
            astra_services::runs::AtomicRunActionAdmission::Started { .. }
        ));
        run_engine
            .persist_typed_cancellation_fixture(
                "user",
                "session",
                "orphaned-grant",
                &[astra_core::STATUS_RUNNING],
                astra_turn_core::orchestration_types::CancellationOrigin::User,
            )
            .await
            .unwrap();

        assert!(matches!(
            ledger
                .dispatch_prepared_with_admission(
                    &invocation,
                    Some(DurableDispatchAdmission {
                        expected_control_epoch: -1,
                        expected_owner_generation: 0,
                    }),
                )
                .await,
            Err(RuntimeInvocationLedgerError::ProcessLocalActionInactive { .. })
        ));
        let record = ledger.get(&invocation).await.unwrap().unwrap();
        assert_eq!(record.state, ToolInvocationState::Prepared);
        assert_eq!(record.attempt_count, 0);
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
        assert_eq!(metadata["side_effects_maybe"], true);
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
            RuntimeToolInvocationLedger::InMemory { ledger, .. } => {
                let reconciled = ledger
                    .run_ledger(&identity)
                    .lock()
                    .await
                    .reconcile_expired_dispatch(&identity, u64::MAX)
                    .unwrap();
                assert_eq!(reconciled.state, ToolInvocationState::OutcomeUnknown);
            }
            RuntimeToolInvocationLedger::Database { .. } => {
                unreachable!("test ledger is in-memory")
            }
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
