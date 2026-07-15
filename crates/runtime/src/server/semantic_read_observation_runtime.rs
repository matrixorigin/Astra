//! Runtime adapter for the bounded semantic read-observation contract.
//!
//! The adapter keeps database and local/offline execution on the same fill,
//! fencing, and capacity state machine. It does not decide eligibility or
//! freshness and it never retries a provider call.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use astra_turn_core::semantic_read_cache::{
    InMemorySemanticReadObservationStore, SemanticReadObservationStoreError,
};
use astra_turn_types::{
    SemanticReadCacheKey, SemanticReadCacheLimits, SemanticReadCacheLookup,
    SemanticReadObservation, ToolInvocationIdentity,
};
use thiserror::Error;

pub(crate) const SEMANTIC_READ_FILL_LEASE_DURATION: Duration = Duration::from_secs(90);

#[derive(Clone)]
pub(crate) enum RuntimeSemanticReadObservationStore {
    Database(astra_services::semantic_read_observation_store::DatabaseSemanticReadObservationStore),
    InMemory(Arc<tokio::sync::Mutex<InMemorySemanticReadObservationStore>>),
}

pub(crate) struct SemanticReadFillClaim {
    store: RuntimeSemanticReadObservationStore,
    key: SemanticReadCacheKey,
    owner_id: String,
}

pub(crate) enum SemanticReadBeforeDispatch {
    Proceed(Option<Box<SemanticReadFillClaim>>),
    Return(astra_tools::ToolResult),
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

pub(crate) async fn before_dispatch(
    store: Option<&RuntimeSemanticReadObservationStore>,
    ledger: &crate::server::tool_invocation_runtime::RuntimeToolInvocationLedger,
    identity: &ToolInvocationIdentity,
    key: Option<&SemanticReadCacheKey>,
) -> SemanticReadBeforeDispatch {
    let Some(key) = key else {
        return SemanticReadBeforeDispatch::Proceed(None);
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
        return SemanticReadBeforeDispatch::Proceed(None);
    };
    let fill_owner = uuid::Uuid::now_v7().to_string();
    match store.lookup_or_claim(identity, key, &fill_owner).await {
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
                    SemanticReadBeforeDispatch::Proceed(None)
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
                    SemanticReadBeforeDispatch::Proceed(None)
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
            SemanticReadBeforeDispatch::Proceed(Some(Box::new(SemanticReadFillClaim {
                store: store.clone(),
                key: key.clone(),
                owner_id: fill_owner,
            })))
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
                "semantic read fill is already in progress; executing this pure read uncached"
            );
            SemanticReadBeforeDispatch::Proceed(None)
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
            SemanticReadBeforeDispatch::Proceed(None)
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
            SemanticReadBeforeDispatch::Proceed(None)
        }
    }
}

pub(crate) async fn settle_fill(
    ledger: &crate::server::tool_invocation_runtime::RuntimeToolInvocationLedger,
    identity: &ToolInvocationIdentity,
    fill: Box<SemanticReadFillClaim>,
    outcome: astra_turn_types::ToolInvocationTerminalOutcome,
    revalidated_key: Option<&SemanticReadCacheKey>,
) {
    if !matches!(
        outcome,
        astra_turn_types::ToolInvocationTerminalOutcome::Succeeded { .. }
    ) {
        abandon_fill(identity, &fill, "route outcome was not successful").await;
        return;
    }
    if revalidated_key != Some(&fill.key) {
        tracing::warn!(
            user_id = %identity.user_id,
            session_id = %identity.session_id,
            run_id = %identity.run_id,
            turn_chain_id = %identity.turn_chain_id,
            invocation_id = %identity.invocation_id,
            semantic_read_cache_state = "freshness_changed_during_dispatch",
            semantic_read_cache_key_id = %fill.key.key_id,
            revalidated_cache_key_id = revalidated_key.map(|key| key.key_id.as_str()),
            "provider freshness changed or became unavailable during dispatch; observation will not be published"
        );
        abandon_fill(
            identity,
            &fill,
            "freshness changed or became unavailable during dispatch",
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
    match fill
        .store
        .complete_fill(identity, &fill.key, &fill.owner_id, &observation)
        .await
    {
        Ok(()) => tracing::debug!(
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
        Err(error) => {
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
    if let Err(error) = fill
        .store
        .abandon_fill(identity, &fill.key, &fill.owner_id)
        .await
    {
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
}
