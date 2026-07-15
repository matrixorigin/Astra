//! Runtime adapter for the bounded semantic read-observation contract.
//!
//! The adapter keeps database and local/offline execution on the same fill,
//! fencing, and capacity state machine. It does not decide eligibility or
//! freshness and it never retries a provider call.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use astra_turn_core::semantic_read_cache::{
    InMemorySemanticReadObservationStore, SemanticReadObservationStoreError,
};
use astra_turn_types::{
    SemanticReadCacheKey, SemanticReadCacheLimits, SemanticReadCacheLookup,
    SemanticReadObservation, ToolInvocationIdentity,
};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

pub(crate) const SEMANTIC_READ_FILL_LEASE_DURATION: Duration = Duration::from_secs(90);
const SEMANTIC_READ_CACHE_OPERATION_DEADLINE: Duration = Duration::from_millis(500);
const SEMANTIC_READ_FILL_WAIT_DEADLINE: Duration = Duration::from_secs(1);
const SEMANTIC_READ_FILL_WAIT_INITIAL_BACKOFF: Duration = Duration::from_millis(25);
const SEMANTIC_READ_FILL_WAIT_MAX_BACKOFF: Duration = Duration::from_millis(250);
const SEMANTIC_READ_MAX_PROCESS_WAITERS: usize = 128;

fn semantic_read_waiters() -> &'static Arc<tokio::sync::Semaphore> {
    static WAITERS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    WAITERS.get_or_init(|| {
        Arc::new(tokio::sync::Semaphore::new(
            SEMANTIC_READ_MAX_PROCESS_WAITERS,
        ))
    })
}
const SEMANTIC_READ_FILL_RENEW_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub(crate) enum RuntimeSemanticReadObservationStore {
    Database(astra_services::semantic_read_observation_store::DatabaseSemanticReadObservationStore),
    InMemory(Arc<tokio::sync::Mutex<InMemorySemanticReadObservationStore>>),
}

pub(crate) struct SemanticReadFillClaim {
    store: RuntimeSemanticReadObservationStore,
    key: SemanticReadCacheKey,
    owner_id: String,
    heartbeat: SemanticReadFillHeartbeat,
}

struct SemanticReadFillHeartbeat {
    cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl SemanticReadFillHeartbeat {
    async fn stop(mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                tracing::warn!(%error, "semantic read fill heartbeat task failed");
            }
        }
    }
}

impl Drop for SemanticReadFillHeartbeat {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub(crate) enum SemanticReadBeforeDispatch {
    Proceed {
        fill: Option<Box<SemanticReadFillClaim>>,
        evidence: Option<SemanticReadDecisionEvidence>,
    },
    Return(astra_tools::ToolResult),
}

pub(crate) struct SemanticReadDecisionEvidence {
    pub(crate) state: &'static str,
    pub(crate) key_id: Option<String>,
}

fn proceed(
    fill: Option<Box<SemanticReadFillClaim>>,
    state: &'static str,
    key: &SemanticReadCacheKey,
) -> SemanticReadBeforeDispatch {
    SemanticReadBeforeDispatch::Proceed {
        fill,
        evidence: Some(SemanticReadDecisionEvidence {
            state,
            key_id: Some(key.key_id.clone()),
        }),
    }
}

impl RuntimeSemanticReadObservationStore {
    pub(crate) fn new(
        pool: Option<astra_core::SharedPool>,
        limits: SemanticReadCacheLimits,
    ) -> Result<Self, RuntimeSemanticReadObservationStoreError> {
        match pool {
            Some(pool) => Ok(Self::Database(
                astra_services::semantic_read_observation_store::DatabaseSemanticReadObservationStore::new(
                    pool, limits,
                )?,
            )),
            None => Ok(Self::InMemory(Arc::new(tokio::sync::Mutex::new(
                InMemorySemanticReadObservationStore::new(limits)?,
            )))),
        }
    }

    pub(crate) async fn lookup_or_claim(
        &self,
        identity: &ToolInvocationIdentity,
        key: &SemanticReadCacheKey,
        fill_owner: &str,
    ) -> Result<SemanticReadCacheLookup, RuntimeSemanticReadObservationStoreError> {
        match self {
            Self::Database(store) => Ok(store
                .lookup_or_claim(
                    &identity.user_id,
                    &identity.session_id,
                    key,
                    fill_owner,
                    duration_millis(SEMANTIC_READ_FILL_LEASE_DURATION)?,
                )
                .await?),
            Self::InMemory(store) => {
                let now = now_epoch_ms()?;
                let expires_at = now
                    .checked_add(duration_millis(SEMANTIC_READ_FILL_LEASE_DURATION)?)
                    .ok_or(RuntimeSemanticReadObservationStoreError::ClockOverflow)?;
                Ok(store
                    .lock()
                    .await
                    .lookup_or_claim(key, fill_owner, expires_at, now)?)
            }
        }
    }

    pub(crate) async fn complete_fill(
        &self,
        identity: &ToolInvocationIdentity,
        key: &SemanticReadCacheKey,
        fill_owner: &str,
        observation: &SemanticReadObservation,
    ) -> Result<(), RuntimeSemanticReadObservationStoreError> {
        match self {
            Self::Database(store) => Ok(store
                .complete_fill(
                    &identity.user_id,
                    &identity.session_id,
                    key,
                    fill_owner,
                    observation,
                )
                .await?),
            Self::InMemory(store) => Ok(store.lock().await.complete_fill(
                key,
                fill_owner,
                now_epoch_ms()?,
                observation.clone(),
            )?),
        }
    }

    async fn renew_fill(
        &self,
        identity: &ToolInvocationIdentity,
        key: &SemanticReadCacheKey,
        fill_owner: &str,
    ) -> Result<(), RuntimeSemanticReadObservationStoreError> {
        match self {
            Self::Database(store) => Ok(store
                .renew_fill(
                    &identity.user_id,
                    &identity.session_id,
                    key,
                    fill_owner,
                    duration_millis(SEMANTIC_READ_FILL_LEASE_DURATION)?,
                )
                .await?),
            Self::InMemory(store) => {
                let now = now_epoch_ms()?;
                let expires_at = now
                    .checked_add(duration_millis(SEMANTIC_READ_FILL_LEASE_DURATION)?)
                    .ok_or(RuntimeSemanticReadObservationStoreError::ClockOverflow)?;
                Ok(store
                    .lock()
                    .await
                    .renew_fill(key, fill_owner, expires_at, now)?)
            }
        }
    }

    pub(crate) async fn abandon_fill(
        &self,
        identity: &ToolInvocationIdentity,
        key: &SemanticReadCacheKey,
        fill_owner: &str,
    ) -> Result<(), RuntimeSemanticReadObservationStoreError> {
        match self {
            Self::Database(store) => Ok(store
                .abandon_fill(&identity.user_id, &identity.session_id, key, fill_owner)
                .await?),
            Self::InMemory(store) => Ok(store.lock().await.abandon_fill(key, fill_owner)?),
        }
    }
}

fn start_fill_heartbeat(
    store: RuntimeSemanticReadObservationStore,
    identity: ToolInvocationIdentity,
    key: SemanticReadCacheKey,
    owner_id: String,
) -> SemanticReadFillHeartbeat {
    let cancel = CancellationToken::new();
    let heartbeat_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(SEMANTIC_READ_FILL_RENEW_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            tokio::select! {
                _ = heartbeat_cancel.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(error) = store.renew_fill(&identity, &key, &owner_id).await {
                        tracing::warn!(
                            user_id = %identity.user_id,
                            session_id = %identity.session_id,
                            run_id = %identity.run_id,
                            turn_chain_id = %identity.turn_chain_id,
                            invocation_id = %identity.invocation_id,
                            semantic_read_cache_key_id = %key.key_id,
                            semantic_read_fill_owner = %owner_id,
                            %error,
                            "semantic read fill lease renewal failed"
                        );
                    }
                }
            }
        }
    });
    SemanticReadFillHeartbeat {
        cancel,
        task: Some(task),
    }
}

async fn lookup_or_wait_for_fill(
    store: &RuntimeSemanticReadObservationStore,
    identity: &ToolInvocationIdentity,
    key: &SemanticReadCacheKey,
    fill_owner: &str,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
) -> Result<SemanticReadCacheLookup, RuntimeSemanticReadObservationStoreError> {
    let deadline = tokio::time::Instant::now() + SEMANTIC_READ_FILL_WAIT_DEADLINE;
    let mut backoff = SEMANTIC_READ_FILL_WAIT_INITIAL_BACKOFF;
    let mut waiter_permit = None;
    loop {
        let lookup = tokio::time::timeout(
            SEMANTIC_READ_CACHE_OPERATION_DEADLINE,
            store.lookup_or_claim(identity, key, fill_owner),
        )
        .await
        .map_err(|_| RuntimeSemanticReadObservationStoreError::OperationTimedOut)??;
        let SemanticReadCacheLookup::FillInProgress {
            lease_expires_at_epoch_ms,
        } = lookup
        else {
            return Ok(lookup);
        };
        if waiter_permit.is_none() {
            waiter_permit = Some(
                semantic_read_waiters()
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| {
                        RuntimeSemanticReadObservationStoreError::WaiterCapacityExceeded
                    })?,
            );
        }
        let now = now_epoch_ms()?;
        let remaining_lease = Duration::from_millis(lease_expires_at_epoch_ms.saturating_sub(now));
        let remaining_wait = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining_lease.is_zero() || remaining_wait.is_zero() {
            return Ok(SemanticReadCacheLookup::FillInProgress {
                lease_expires_at_epoch_ms,
            });
        }
        let sleep = tokio::time::sleep(backoff.min(remaining_lease).min(remaining_wait));
        if let Some(cancel_token) = cancel_token {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    return Err(RuntimeSemanticReadObservationStoreError::WaitCancelled);
                }
                _ = sleep => {}
            }
        } else {
            sleep.await;
        }
        backoff = backoff
            .saturating_mul(2)
            .min(SEMANTIC_READ_FILL_WAIT_MAX_BACKOFF);
    }
}

pub(crate) async fn before_dispatch(
    store: Option<&RuntimeSemanticReadObservationStore>,
    ledger: &crate::server::tool_invocation_runtime::RuntimeToolInvocationLedger,
    identity: &ToolInvocationIdentity,
    key: Option<&SemanticReadCacheKey>,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
) -> SemanticReadBeforeDispatch {
    let Some(key) = key else {
        return SemanticReadBeforeDispatch::Proceed {
            fill: None,
            evidence: None,
        };
    };
    let Some(store) = store else {
        tracing::debug!(
            user_id = %identity.user_id,
            session_id = %identity.session_id,
            run_id = %identity.run_id,
            turn_chain_id = %identity.turn_chain_id,
            invocation_id = %identity.invocation_id,
            semantic_read_cache_state = "rollout_disabled",
            semantic_read_cache_key_id = %key.key_id,
            "semantic read observation cache is not enabled for this runtime"
        );
        return proceed(None, "rollout_disabled", key);
    };
    let fill_owner = uuid::Uuid::now_v7().to_string();
    match lookup_or_wait_for_fill(store, identity, key, &fill_owner, cancel_token).await {
        Ok(SemanticReadCacheLookup::Hit(observation)) => {
            match ledger
                .complete_from_semantic_read_cache(identity, key, &observation)
                .await
            {
                Ok(Some(result)) => {
                    tracing::debug!(
                        user_id = %identity.user_id,
                        session_id = %identity.session_id,
                        run_id = %identity.run_id,
                        turn_chain_id = %identity.turn_chain_id,
                        invocation_id = %identity.invocation_id,
                        semantic_read_cache_state = "hit",
                        semantic_read_cache_key_id = %key.key_id,
                        semantic_read_observation_id = %observation.observation_id,
                        "completed logical invocation from semantic read observation"
                    );
                    SemanticReadBeforeDispatch::Return(result)
                }
                Ok(None) => {
                    tracing::warn!(
                        user_id = %identity.user_id,
                        session_id = %identity.session_id,
                        run_id = %identity.run_id,
                        turn_chain_id = %identity.turn_chain_id,
                        invocation_id = %identity.invocation_id,
                        semantic_read_cache_state = "completion_degraded",
                        semantic_read_cache_key_id = %key.key_id,
                        "cache completion failed but the ledger authoritatively remained prepared; dispatching normally"
                    );
                    proceed(None, "completion_degraded", key)
                }
                Err(error) => {
                    tracing::warn!(
                        user_id = %identity.user_id,
                        session_id = %identity.session_id,
                        run_id = %identity.run_id,
                        turn_chain_id = %identity.turn_chain_id,
                        invocation_id = %identity.invocation_id,
                        semantic_read_cache_state = "completion_failed",
                        semantic_read_cache_key_id = %key.key_id,
                        %error,
                        "semantic cache completion failed; the authoritative dispatch CAS will decide whether execution is still needed"
                    );
                    proceed(None, "completion_failed", key)
                }
            }
        }
        Ok(SemanticReadCacheLookup::FillClaimed) => {
            tracing::debug!(
                user_id = %identity.user_id,
                session_id = %identity.session_id,
                run_id = %identity.run_id,
                turn_chain_id = %identity.turn_chain_id,
                invocation_id = %identity.invocation_id,
                semantic_read_cache_state = "fill_claimed",
                semantic_read_cache_key_id = %key.key_id,
                semantic_read_fill_owner = %fill_owner,
                "claimed semantic read observation fill"
            );
            let heartbeat = start_fill_heartbeat(
                store.clone(),
                identity.clone(),
                key.clone(),
                fill_owner.clone(),
            );
            proceed(
                Some(Box::new(SemanticReadFillClaim {
                    store: store.clone(),
                    key: key.clone(),
                    owner_id: fill_owner,
                    heartbeat,
                })),
                "fill_claimed",
                key,
            )
        }
        Ok(SemanticReadCacheLookup::FillInProgress {
            lease_expires_at_epoch_ms,
        }) => {
            tracing::debug!(
                user_id = %identity.user_id,
                session_id = %identity.session_id,
                run_id = %identity.run_id,
                turn_chain_id = %identity.turn_chain_id,
                invocation_id = %identity.invocation_id,
                semantic_read_cache_state = "fill_in_progress",
                semantic_read_cache_key_id = %key.key_id,
                lease_expires_at_epoch_ms,
                semantic_read_fill_wait_deadline_ms = SEMANTIC_READ_FILL_WAIT_DEADLINE.as_millis(),
                "semantic read fill did not settle within the bounded wait; executing this pure read uncached"
            );
            proceed(None, "fill_in_progress", key)
        }
        Ok(SemanticReadCacheLookup::FillCapacityExceeded) => {
            tracing::warn!(
                user_id = %identity.user_id,
                session_id = %identity.session_id,
                run_id = %identity.run_id,
                turn_chain_id = %identity.turn_chain_id,
                invocation_id = %identity.invocation_id,
                semantic_read_cache_state = "fill_capacity_exceeded",
                semantic_read_cache_key_id = %key.key_id,
                "semantic read fill capacity is saturated; executing this pure read uncached"
            );
            proceed(None, "fill_capacity_exceeded", key)
        }
        Err(RuntimeSemanticReadObservationStoreError::WaitCancelled) => {
            tracing::debug!(
                user_id = %identity.user_id,
                session_id = %identity.session_id,
                run_id = %identity.run_id,
                turn_chain_id = %identity.turn_chain_id,
                invocation_id = %identity.invocation_id,
                semantic_read_cache_state = "wait_cancelled",
                semantic_read_cache_key_id = %key.key_id,
                "semantic read wait was cancelled; authoritative route cancellation remains in control"
            );
            proceed(None, "wait_cancelled", key)
        }
        Err(error) => {
            tracing::warn!(
                user_id = %identity.user_id,
                session_id = %identity.session_id,
                run_id = %identity.run_id,
                turn_chain_id = %identity.turn_chain_id,
                invocation_id = %identity.invocation_id,
                semantic_read_cache_state = "lookup_degraded",
                semantic_read_cache_key_id = %key.key_id,
                %error,
                "semantic read observation lookup failed; executing normally"
            );
            proceed(None, "lookup_degraded", key)
        }
    }
}

pub(crate) async fn settle_fill(
    ledger: &crate::server::tool_invocation_runtime::RuntimeToolInvocationLedger,
    identity: &ToolInvocationIdentity,
    mut fill: Box<SemanticReadFillClaim>,
    outcome: astra_turn_types::ToolInvocationTerminalOutcome,
    provider_confirmed: bool,
) {
    let heartbeat = std::mem::replace(
        &mut fill.heartbeat,
        SemanticReadFillHeartbeat {
            cancel: CancellationToken::new(),
            task: None,
        },
    );
    heartbeat.stop().await;
    if !matches!(
        outcome,
        astra_turn_types::ToolInvocationTerminalOutcome::Succeeded { .. }
    ) {
        abandon_fill(identity, &fill, "route outcome was not successful").await;
        return;
    }
    if !provider_confirmed {
        tracing::warn!(
            user_id = %identity.user_id,
            session_id = %identity.session_id,
            run_id = %identity.run_id,
            turn_chain_id = %identity.turn_chain_id,
            invocation_id = %identity.invocation_id,
            semantic_read_cache_state = "condition_unconfirmed",
            semantic_read_cache_key_id = %fill.key.key_id,
            "provider did not confirm the exact conditional read; observation will not be published"
        );
        abandon_fill(
            identity,
            &fill,
            "provider did not confirm the exact conditional read",
        )
        .await;
        return;
    }
    let observation =
        match SemanticReadObservation::from_terminal_outcome(fill.key.clone(), &outcome) {
            Ok(observation) => observation,
            Err(error) => {
                tracing::warn!(
                    user_id = %identity.user_id,
                    session_id = %identity.session_id,
                    run_id = %identity.run_id,
                    turn_chain_id = %identity.turn_chain_id,
                    invocation_id = %identity.invocation_id,
                    semantic_read_cache_state = "observation_rejected",
                    semantic_read_cache_key_id = %fill.key.key_id,
                    %error,
                    "successful read result was not retained as a semantic observation"
                );
                abandon_fill(
                    identity,
                    &fill,
                    "semantic observation contract rejected the result",
                )
                .await;
                return;
            }
        };
    match ledger.confirms_dispatched_outcome(identity, &outcome).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(
                user_id = %identity.user_id,
                session_id = %identity.session_id,
                run_id = %identity.run_id,
                turn_chain_id = %identity.turn_chain_id,
                invocation_id = %identity.invocation_id,
                semantic_read_cache_state = "durability_unconfirmed",
                semantic_read_cache_key_id = %fill.key.key_id,
                "read result did not match a durable dispatched terminal outcome; observation will not be published"
            );
            abandon_fill(
                identity,
                &fill,
                "durable dispatched outcome was not confirmed",
            )
            .await;
            return;
        }
        Err(error) => {
            tracing::warn!(
                user_id = %identity.user_id,
                session_id = %identity.session_id,
                run_id = %identity.run_id,
                turn_chain_id = %identity.turn_chain_id,
                invocation_id = %identity.invocation_id,
                semantic_read_cache_state = "durability_check_failed",
                semantic_read_cache_key_id = %fill.key.key_id,
                %error,
                "could not confirm durable read outcome; observation will not be published"
            );
            abandon_fill(identity, &fill, "durable outcome confirmation failed").await;
            return;
        }
    }
    match tokio::time::timeout(
        SEMANTIC_READ_CACHE_OPERATION_DEADLINE,
        fill.store
            .complete_fill(identity, &fill.key, &fill.owner_id, &observation),
    )
    .await
    {
        Err(_) => {
            tracing::warn!(
                user_id = %identity.user_id,
                session_id = %identity.session_id,
                run_id = %identity.run_id,
                turn_chain_id = %identity.turn_chain_id,
                invocation_id = %identity.invocation_id,
                semantic_read_cache_state = "fill_timeout",
                semantic_read_cache_key_id = %fill.key.key_id,
                "semantic observation publication exceeded its independent deadline; lease expiry remains the fence"
            );
        }
        Ok(Ok(())) => tracing::debug!(
            user_id = %identity.user_id,
            session_id = %identity.session_id,
            run_id = %identity.run_id,
            turn_chain_id = %identity.turn_chain_id,
            invocation_id = %identity.invocation_id,
            semantic_read_cache_state = "filled",
            semantic_read_cache_key_id = %fill.key.key_id,
            semantic_read_observation_id = %observation.observation_id,
            "published durable semantic read observation"
        ),
        Ok(Err(error)) => {
            tracing::warn!(
                user_id = %identity.user_id,
                session_id = %identity.session_id,
                run_id = %identity.run_id,
                turn_chain_id = %identity.turn_chain_id,
                invocation_id = %identity.invocation_id,
                semantic_read_cache_state = "fill_failed",
                semantic_read_cache_key_id = %fill.key.key_id,
                %error,
                "durable invocation succeeded but semantic observation publication failed"
            );
            abandon_fill(identity, &fill, "semantic observation publication failed").await;
        }
    }
}

pub(crate) async fn abandon_fill(
    identity: &ToolInvocationIdentity,
    fill: &SemanticReadFillClaim,
    reason: &'static str,
) {
    let abandoned = tokio::time::timeout(
        SEMANTIC_READ_CACHE_OPERATION_DEADLINE,
        fill.store.abandon_fill(identity, &fill.key, &fill.owner_id),
    )
    .await;
    let error = match abandoned {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error.to_string()),
        Err(_) => Some("operation timed out".to_string()),
    };
    if let Some(error) = error {
        tracing::warn!(
            user_id = %identity.user_id,
            session_id = %identity.session_id,
            run_id = %identity.run_id,
            turn_chain_id = %identity.turn_chain_id,
            invocation_id = %identity.invocation_id,
            semantic_read_cache_state = "abandon_failed",
            semantic_read_cache_key_id = %fill.key.key_id,
            semantic_read_fill_owner = %fill.owner_id,
            reason,
            %error,
            "semantic read fill could not be abandoned; lease expiry will fence it"
        );
    } else {
        tracing::debug!(
            user_id = %identity.user_id,
            session_id = %identity.session_id,
            run_id = %identity.run_id,
            turn_chain_id = %identity.turn_chain_id,
            invocation_id = %identity.invocation_id,
            semantic_read_cache_state = "fill_abandoned",
            semantic_read_cache_key_id = %fill.key.key_id,
            semantic_read_fill_owner = %fill.owner_id,
            reason,
            "semantic read fill abandoned"
        );
    }
}

fn now_epoch_ms() -> Result<u64, RuntimeSemanticReadObservationStoreError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| RuntimeSemanticReadObservationStoreError::Clock(error.to_string()))?
        .as_millis();
    u64::try_from(millis).map_err(|_| RuntimeSemanticReadObservationStoreError::ClockOverflow)
}

fn duration_millis(duration: Duration) -> Result<u64, RuntimeSemanticReadObservationStoreError> {
    u64::try_from(duration.as_millis())
        .map_err(|_| RuntimeSemanticReadObservationStoreError::ClockOverflow)
}

#[derive(Debug, Error)]
pub(crate) enum RuntimeSemanticReadObservationStoreError {
    #[error(transparent)]
    Database(
        #[from] astra_services::semantic_read_observation_store::SemanticReadObservationStoreError,
    ),
    #[error(transparent)]
    InMemory(#[from] SemanticReadObservationStoreError),
    #[error("semantic read observation clock failed: {0}")]
    Clock(String),
    #[error("semantic read observation clock overflow")]
    ClockOverflow,
    #[error("semantic read cache operation exceeded its independent deadline")]
    OperationTimedOut,
    #[error("semantic read cache process waiter capacity is exhausted")]
    WaiterCapacityExceeded,
    #[error("semantic read cache wait was cancelled")]
    WaitCancelled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_types::{
        DurableToolReference, SemanticFreshnessFact, SemanticFreshnessScope,
        SemanticReadFreshnessContext, ToolInvocationResultPayload, ToolInvocationTerminalOutcome,
    };
    use std::collections::BTreeMap;

    fn identity(invocation_id: &str) -> ToolInvocationIdentity {
        ToolInvocationIdentity::new("user-1", "session-1", "run-1", "turn-1", invocation_id)
            .unwrap()
    }

    fn key() -> SemanticReadCacheKey {
        let freshness = SemanticReadFreshnessContext::new(
            "user-1:session-1",
            vec![
                SemanticFreshnessFact::new(
                    SemanticFreshnessScope::Resource,
                    "resource-1",
                    "revision-1",
                )
                .unwrap(),
            ],
        )
        .unwrap();
        SemanticReadCacheKey::new(
            DurableToolReference::built_in("introspect", "v1").unwrap(),
            &serde_json::json!({"query": "status"}),
            &format!("sha256:{:064x}", 7),
            &freshness,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn same_key_waiter_reuses_the_first_fill_instead_of_dispatching() {
        let store =
            RuntimeSemanticReadObservationStore::new(None, SemanticReadCacheLimits::default())
                .unwrap();
        let key = key();
        let first_identity = identity("call-1");
        assert!(matches!(
            store
                .lookup_or_claim(&first_identity, &key, "owner-1")
                .await
                .unwrap(),
            SemanticReadCacheLookup::FillClaimed
        ));

        let waiter_store = store.clone();
        let waiter_key = key.clone();
        let waiter_identity = identity("call-2");
        let waiter = tokio::spawn(async move {
            lookup_or_wait_for_fill(
                &waiter_store,
                &waiter_identity,
                &waiter_key,
                "owner-2",
                None,
            )
            .await
            .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(75)).await;

        let outcome = ToolInvocationTerminalOutcome::Succeeded {
            result: ToolInvocationResultPayload {
                output: "provider result".to_string(),
                metadata: BTreeMap::new(),
                exit_semantics: None,
            },
        };
        let observation =
            SemanticReadObservation::from_terminal_outcome(key.clone(), &outcome).unwrap();
        store
            .complete_fill(&first_identity, &key, "owner-1", &observation)
            .await
            .unwrap();

        assert!(matches!(
            waiter.await.unwrap(),
            SemanticReadCacheLookup::Hit(hit) if *hit == observation
        ));
    }

    #[tokio::test]
    async fn same_key_waiter_honors_cancellation_without_disturbing_fill_owner() {
        let store =
            RuntimeSemanticReadObservationStore::new(None, SemanticReadCacheLimits::default())
                .unwrap();
        let key = key();
        let first_identity = identity("call-owner");
        assert_eq!(
            store
                .lookup_or_claim(&first_identity, &key, "owner-1")
                .await
                .unwrap(),
            SemanticReadCacheLookup::FillClaimed
        );
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        assert!(matches!(
            lookup_or_wait_for_fill(
                &store,
                &identity("call-waiter"),
                &key,
                "owner-2",
                Some(&cancellation),
            )
            .await,
            Err(RuntimeSemanticReadObservationStoreError::WaitCancelled)
        ));
        assert!(matches!(
            store
                .lookup_or_claim(&first_identity, &key, "owner-3")
                .await
                .unwrap(),
            SemanticReadCacheLookup::FillInProgress { .. }
        ));
    }

    #[tokio::test]
    async fn disabled_store_returns_bounded_explainable_bypass_evidence() {
        let key = key();
        let ledger = crate::server::tool_invocation_runtime::RuntimeToolInvocationLedger::new(None);
        let decision =
            before_dispatch(None, &ledger, &identity("call-disabled"), Some(&key), None).await;
        let SemanticReadBeforeDispatch::Proceed {
            fill: None,
            evidence: Some(evidence),
        } = decision
        else {
            panic!("disabled cache must execute authoritatively with evidence");
        };
        assert_eq!(evidence.state, "rollout_disabled");
        assert_eq!(evidence.key_id, Some(key.key_id));
    }
}
