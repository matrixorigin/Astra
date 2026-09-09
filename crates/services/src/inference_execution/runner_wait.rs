//! Bounded, read-only readiness hints. A hint never grants continuation authority:
//! callers must still claim the exact attempt transactionally in `runner`.

use crate::{ServiceError, ServiceResult};
use astra_core::SharedPool;
use astra_turn_types::runner_inference::RunnerInferenceAttemptIdentity;
use sqlx::{MySql, Row};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::sync::{Notify, watch};

type Key = (String, String);
const MAX_WAITERS: usize = 1024;
const MAX_WAITERS_PER_USER: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunnerReadiness {
    Waiting,
    Ready,
    Unavailable,
}

#[derive(Default, Debug)]
struct State {
    running: bool,
    waiters: HashMap<Key, watch::Sender<RunnerReadiness>>,
    reservations: HashMap<String, usize>,
}

#[derive(Default)]
pub struct RunnerContinuationWaiters {
    state: Mutex<State>,
    wakeup: Notify,
    // A coordinator belongs to one AppState database. Never let an accidental
    // cross-pool reuse turn an identically named attempt into a readiness hint.
    pool: OnceLock<SharedPool>,
}

impl std::fmt::Debug for RunnerContinuationWaiters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunnerContinuationWaiters")
            .finish_non_exhaustive()
    }
}

#[must_use = "reserve result observation before authorizing a provider request"]
pub struct RunnerReadinessReservation {
    owner: Arc<RunnerContinuationWaiters>,
    user_id: String,
}

impl Drop for RunnerReadinessReservation {
    fn drop(&mut self) {
        let mut state = self
            .owner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = state.reservations.get_mut(&self.user_id) {
            *count -= 1;
            if *count == 0 {
                state.reservations.remove(&self.user_id);
            }
        }
    }
}

impl RunnerReadinessReservation {
    pub fn subscribe(
        self,
        identity: &RunnerInferenceAttemptIdentity,
    ) -> ServiceResult<watch::Receiver<RunnerReadiness>> {
        if identity.user_id != self.user_id {
            return Err(ServiceError::invalid(
                "Runner readiness reservation owner mismatch",
            ));
        }
        let pool = self
            .owner
            .pool
            .get()
            .ok_or_else(|| ServiceError::internal("Runner readiness database is not bound"))?
            .clone();
        let key = (
            self.user_id.clone(),
            identity.attempt_id.as_str().to_owned(),
        );
        let mut state = self
            .owner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(sender) = state.waiters.get(&key) {
            return Ok(sender.subscribe());
        }
        let (sender, receiver) = watch::channel(RunnerReadiness::Waiting);
        state.waiters.insert(key, sender);
        if !state.running {
            state.running = true;
            let owner = self.owner.clone();
            tokio::spawn(async move {
                owner.run(pool).await;
            });
        }
        self.owner.notify();
        // The lock is dropped before self releases its reservation.
        Ok(receiver)
    }
}

impl RunnerContinuationWaiters {
    pub fn notify(&self) {
        self.wakeup.notify_one();
    }

    pub async fn subscribe(
        self: &Arc<Self>,
        pool: SharedPool,
        identity: &RunnerInferenceAttemptIdentity,
    ) -> ServiceResult<watch::Receiver<RunnerReadiness>> {
        self.reserve(&pool, &identity.user_id)?.subscribe(identity)
    }

    pub fn reserve(
        self: &Arc<Self>,
        pool: &SharedPool,
        user_id: &str,
    ) -> ServiceResult<RunnerReadinessReservation> {
        let bound = self.pool.get_or_init(|| pool.clone());
        if !std::ptr::eq(bound.get(), pool.get()) {
            return Err(ServiceError::invalid(
                "Runner readiness coordinator belongs to another database pool",
            ));
        }
        self.reserve_for(user_id)
    }

    fn reserve_for(self: &Arc<Self>, user_id: &str) -> ServiceResult<RunnerReadinessReservation> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .waiters
            .retain(|_, sender| sender.receiver_count() > 0);
        if state.waiters.len() + state.reservations.values().sum::<usize>() >= MAX_WAITERS
            || state.waiters.keys().filter(|k| k.0 == user_id).count()
                + state.reservations.get(user_id).copied().unwrap_or_default()
                >= MAX_WAITERS_PER_USER
        {
            return Err(ServiceError::new(
                crate::ServiceErrorKind::ConflictTransient,
                "Runner readiness waiter capacity exhausted",
            ));
        }
        *state.reservations.entry(user_id.to_owned()).or_default() += 1;
        Ok(RunnerReadinessReservation {
            owner: self.clone(),
            user_id: user_id.to_owned(),
        })
    }

    async fn run(self: Arc<Self>, pool: SharedPool) {
        loop {
            let keys = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state
                    .waiters
                    .retain(|_, sender| sender.receiver_count() > 0);
                if state.waiters.is_empty() {
                    // Subscription and the idle transition share the same lock.
                    state.running = false;
                    return;
                }
                state.waiters.keys().cloned().collect::<Vec<_>>()
            };
            let poll_started = tokio::time::Instant::now();
            for batch in keys.chunks(128) {
                match tokio::time::timeout(Duration::from_secs(2), poll_batch(&pool, batch)).await {
                    Ok(Ok(ready)) => {
                        let mut state = self
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        for key in batch {
                            let readiness = ready
                                .get(key)
                                .copied()
                                .unwrap_or(RunnerReadiness::Unavailable);
                            if readiness != RunnerReadiness::Waiting
                                && let Some(sender) = state.waiters.remove(key)
                            {
                                let _ = sender.send(readiness);
                            }
                        }
                    }
                    // A transient storage failure is not terminal evidence.
                    // Keep the bounded subscriptions until their caller deadline.
                    Ok(Err(_)) | Err(_) => {
                        // One failed query is enough evidence for this pass;
                        // avoid amplifying a database outage across all batches.
                        tracing::debug!(
                            batch_size = batch.len(),
                            "Runner readiness observation unavailable; retained until caller deadline"
                        );
                        break;
                    }
                }
            }
            // Coalesce notification storms. Regardless of connected users,
            // no coordinator issues more than 20 batched observation passes/s.
            tokio::time::sleep_until(poll_started + Duration::from_millis(50)).await;
            tokio::select! {
                _ = self.wakeup.notified() => {},
                _ = tokio::time::sleep(Duration::from_millis(500)) => {},
            }
        }
    }
}

async fn poll_batch(
    pool: &SharedPool,
    keys: &[Key],
) -> Result<HashMap<Key, RunnerReadiness>, sqlx::Error> {
    let mut query = sqlx::QueryBuilder::<MySql>::new(
        "SELECT user_id, attempt_id, runner_continuation_pending, runner_terminal_conflict FROM inference_provider_attempts WHERE ",
    );
    let mut where_clause = query.separated(" OR ");
    for (user, attempt) in keys {
        where_clause
            .push("(user_id = ")
            .push_bind_unseparated(user)
            .push_unseparated(" AND attempt_id = ")
            .push_bind_unseparated(attempt)
            .push_unseparated(")");
    }
    let rows = query.build().fetch_all(pool.get()).await?;
    rows.iter()
        .map(|row| {
            let readiness = if row.try_get::<bool, _>("runner_terminal_conflict")? {
                RunnerReadiness::Unavailable
            } else if row.try_get::<bool, _>("runner_continuation_pending")? {
                RunnerReadiness::Ready
            } else {
                RunnerReadiness::Waiting
            };
            Ok((
                (row.try_get("user_id")?, row.try_get("attempt_id")?),
                readiness,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_waiter_reservations_bound_each_user_and_release_on_drop() {
        let owner = Arc::new(RunnerContinuationWaiters::default());
        let mut alice = (0..MAX_WAITERS_PER_USER)
            .map(|_| owner.reserve_for("alice").unwrap())
            .collect::<Vec<_>>();
        assert!(owner.reserve_for("alice").is_err());
        let bob = owner.reserve_for("bob").unwrap();
        drop(alice.pop());
        let replacement = owner.reserve_for("alice").unwrap();
        drop((alice, replacement, bob));
        assert!(owner.state.lock().unwrap().reservations.is_empty());
        assert!(owner.reserve_for("alice").is_ok());
    }

    #[test]
    fn runner_waiter_global_capacity_and_cancelled_subscriptions_are_bounded() {
        let owner = Arc::new(RunnerContinuationWaiters::default());
        let held = (0..MAX_WAITERS)
            .map(|index| {
                owner
                    .reserve_for(&format!("user-{}", index / MAX_WAITERS_PER_USER))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(owner.reserve_for("new-user").is_err());
        drop(held);
        let (sender, receiver) = watch::channel(RunnerReadiness::Waiting);
        owner
            .state
            .lock()
            .unwrap()
            .waiters
            .insert(("alice".into(), "old-attempt".into()), sender);
        drop(receiver);
        let reservation = owner.reserve_for("bob").unwrap();
        assert!(owner.state.lock().unwrap().waiters.is_empty());
        drop(reservation);
    }
}
