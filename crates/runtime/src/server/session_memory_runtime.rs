//! Recovery driver for canonical extraction and session-end governance.
use crate::turn::cloud::memoria_compact::UserScopedMemoriaPort;
use crate::{AppState, session_memory::durable::DurableMemoriaPort};
use astra_services::session_memory_lifecycle::SessionMemoryLifecycleStore;
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

pub(crate) fn spawn_session_memory_recovery(
    state: AppState,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Some(pool) = state.shared_pool.clone() else {
            return;
        };
        let store = SessionMemoryLifecycleStore::new(pool);
        let observation = SessionObservationRecovery::new(state.clone());
        // Both loops belong to this managed task. Independent timers keep a
        // slow optional backend from delaying lifecycle maintenance batches.
        let observe = async {
            let Some(observation) = observation else {
                return;
            };
            observation.run(cancel.clone()).await;
        };
        let maintain = async {
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let due = match store.list_due(16).await {
                    Ok(due) => due,
                    Err(error) => {
                        tracing::warn!(%error, "session memory recovery scan failed");
                        continue;
                    }
                };
                use futures_util::{StreamExt, stream};
                stream::iter(due).for_each_concurrent(2, |(user, session)| {
                    let state = state.clone();
                    async move {
                        if let Err(error) = drive_session_memory_work(&state, &user, &session).await {
                            tracing::warn!(session_id = session, %error, "session memory recovery remains pending");
                        }
                    }
                }).await;
            }
        };
        tokio::select! {
            _ = cancel.cancelled() => {},
            _ = async { tokio::join!(observe, maintain); } => {}
        }
    })
}

pub(crate) async fn drive_session_memory_work(
    state: &AppState,
    user: &str,
    session: &str,
) -> Result<(), String> {
    let store = SessionMemoryLifecycleStore::new(
        state
            .shared_pool
            .clone()
            .ok_or("durable memory storage is unavailable")?,
    );
    let Some(claim) = store.claim(user, session).await? else {
        return Ok(());
    };
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        use astra_memoria::MemoriaPort;
        let resolver = state
            .auth_service
            .memoria_credentials()
            .ok_or("memory authority is unavailable")?;
        let mut client = UserScopedMemoriaPort::new(resolver, user.into()).with_session(session);
        if state.memoria_self_hosted_fallback_enabled
            && let Some(key) = state.memoria_master_key.clone()
        {
            client = client.with_self_hosted_fallback(state.memoria_base_url.clone(), key);
        }
        if !client.admits_operation(true).await? {
            return Err("durable memory maintenance awaits current consent".into());
        }
        let mut current_claim = claim.clone();
        if let Some(head) = store.missing_input_head(user, session).await? {
            use astra_services::session_context_coordinator::{
                DatabaseSessionContextCoordinator, SessionContextCoordinator,
            };
            let coordinator = DatabaseSessionContextCoordinator::new(
                state
                    .shared_pool
                    .clone()
                    .ok_or("missing canonical context pool")?,
            );
            let materialized = coordinator
                .materialize(&head)
                .await
                .map_err(|error| error.to_string())?;
            let mut facts: astra_turn_types::session_facts::SessionFacts = claim
                .work
                .latest_facts
                .as_ref()
                .map(|facts| serde_json::from_value(facts.clone()))
                .transpose()
                .map_err(|error| error.to_string())?
                .unwrap_or_default();
            facts.turn = head.cursor.completed_turn;
            let request = crate::session_memory::ExtractionRequest {
                inference_scope: astra_turn_types::InferenceInvocationScope::Session {
                    session_id: session.into(),
                    turn: head.cursor.completed_turn,
                    round: 0,
                    operation_id: "memory_extraction".into(),
                    logical_attempt: 0,
                },
                messages: materialized.messages,
                session_facts: facts,
                had_error: false,
                reanchors_current_objective: false,
            };
            use sha2::{Digest, Sha256};
            let value = serde_json::to_value(&request).map_err(|error| error.to_string())?;
            let fingerprint = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&value).map_err(|error| error.to_string())?)
            );
            store
                .publish_recovery(&claim, value, fingerprint, head.cursor.completed_turn)
                .await?;
            current_claim.work = store.load_claim_work(&claim).await?;
        }
        if current_claim.work.active_request.is_some() {
            let service = state
                .matrix_cloud_runtime
                .as_ref()
                .and_then(|runtime| runtime.clone_memory_extraction_service())
                .ok_or("memory extraction owner is unavailable")?;
            return service
                .scoped_to_session(user, session)?
                .run_durable_claim(current_claim)
                .await;
        }
        let mut client = DurableMemoriaPort {
            inner: Arc::new(client),
            store: store.clone(),
            claim: current_claim,
        };
        let plan = client.prepare_plan().await?;
        client.claim.work.plan =
            Some(serde_json::to_value(&plan).map_err(|error| error.to_string())?);
        let report = crate::turn::cloud::session_end_governance::run_session_end_governance(
            &plan.facts,
            session,
            &client,
        )
        .await?;
        tracing::info!(
            session_id = session,
            consolidation_complete = report.consolidation_complete(),
            reflection_failed = report.reflection_failed,
            scene_failures = report.scene_persist_failures,
            "durable session memory governance finished"
        );
        client.finish_governance(&report).await
    })
    .await;
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            store.release(&claim, false).await?;
            Err(error)
        }
        Err(_) => {
            store.release(&claim, false).await?;
            Err("durable session memory attempt deadline exceeded".into())
        }
    }
}

/// One bounded due-page reader per process feeds eight independent Session
/// attempts. Shared idle cooldown makes empty polling one read per two seconds,
/// not one read per worker. The durable per-Session due marker owns crash retry.
pub(crate) struct SessionObservationRecovery {
    state: AppState,
    store: astra_services::session_observation::SessionObservationStore,
    scope: Option<(String, String)>,
    pending: tokio::sync::Mutex<ObservationPending>,
    failure_budget: std::sync::Arc<std::sync::Mutex<ObservationFailureBudget>>,
}
const OBSERVATION_CONCURRENCY: usize = 8;
#[derive(Default)]
struct ObservationPending {
    keys: std::collections::VecDeque<(String, String)>,
    resume_at: Option<tokio::time::Instant>,
}

/// Process-local health budget for this configured optional observer. Eight
/// consecutive failed attempts open a shared pause before any discovery/claim
/// SQL. One fresh attempt probes recovery; no credentials or work are cached.
#[derive(Default)]
struct ObservationFailureBudget {
    errors: u32,
    in_flight: u32,
    epoch: u64,
    retry_at: Option<tokio::time::Instant>,
    delay_secs: u64,
    probing: bool,
}
struct ObservationHealthPermit {
    budget: std::sync::Arc<std::sync::Mutex<ObservationFailureBudget>>,
    epoch: u64,
    probe: bool,
    finished: bool,
}
impl ObservationFailureBudget {
    fn acquire(shared: &std::sync::Arc<std::sync::Mutex<Self>>) -> Option<ObservationHealthPermit> {
        let mut state = shared.lock().unwrap();
        let probe = state.retry_at.is_some();
        if state.probing
            || state
                .retry_at
                .is_some_and(|at| tokio::time::Instant::now() < at)
        {
            return None;
        }
        if !probe && state.errors + state.in_flight >= OBSERVATION_CONCURRENCY as u32 {
            return None;
        }
        if !probe {
            state.in_flight += 1;
        }
        state.probing = probe;
        Some(ObservationHealthPermit {
            budget: shared.clone(),
            epoch: state.epoch,
            probe,
            finished: false,
        })
    }
}
impl ObservationHealthPermit {
    fn finish(mut self, outcome: Option<bool>) {
        self.finished = true;
        let mut state = self.budget.lock().unwrap();
        if state.epoch != self.epoch {
            return;
        }
        if !self.probe {
            state.in_flight -= 1;
        }
        if self.probe {
            state.probing = false;
            if outcome == Some(true) {
                state.errors = 0;
                state.retry_at = None;
                state.delay_secs = 0;
            } else {
                if outcome == Some(false) {
                    state.delay_secs = (state.delay_secs * 2).min(960);
                }
                // A lost claim or empty page says nothing about dependency
                // health. Continue to the next key; discovery retains its own
                // shared idle cooldown, so this cannot spin on an empty catalog.
                state.retry_at = Some(
                    tokio::time::Instant::now()
                        + Duration::from_secs(if outcome.is_none() {
                            0
                        } else {
                            state.delay_secs
                        }),
                );
            }
            state.epoch = state.epoch.wrapping_add(1);
        } else if outcome == Some(true) {
            state.errors = 0;
        } else if outcome == Some(false) {
            state.errors += 1;
            if state.errors >= OBSERVATION_CONCURRENCY as u32 {
                state.delay_secs = 5;
                state.retry_at = Some(tokio::time::Instant::now() + Duration::from_secs(5));
                state.epoch = state.epoch.wrapping_add(1);
            }
        }
    }
}
impl Drop for ObservationHealthPermit {
    fn drop(&mut self) {
        if !self.finished {
            let mut state = self.budget.lock().unwrap();
            if state.epoch == self.epoch {
                if self.probe {
                    state.probing = false;
                    state.retry_at =
                        Some(tokio::time::Instant::now() + Duration::from_secs(state.delay_secs));
                    state.epoch = state.epoch.wrapping_add(1);
                } else {
                    state.in_flight -= 1;
                }
            }
        }
    }
}

impl SessionObservationRecovery {
    pub(crate) fn new(state: AppState) -> Option<Self> {
        let pool = state.shared_pool.clone()?;
        if !state.turn_persistence.observer_worker.is_configured() {
            return None;
        }
        Some(Self {
            state,
            store: astra_services::session_observation::SessionObservationStore::new(pool),
            scope: None,
            pending: Default::default(),
            failure_budget: Default::default(),
        })
    }
    #[cfg(any(test, feature = "e2e-hooks"))]
    pub(crate) fn scope_for_test(&mut self, user: &str, session: &str) {
        self.scope = Some((user.into(), session.into()));
    }
    #[cfg(any(test, feature = "e2e-hooks"))]
    pub(crate) async fn expire_lease_for_test(&self) {
        self.store.expire_leases_for_test().await;
    }
    #[cfg(any(test, feature = "e2e-hooks"))]
    pub(crate) fn failure_paused_for_test(&self) -> bool {
        let state = self.failure_budget.lock().unwrap();
        state.in_flight == 0
            && state
                .retry_at
                .is_some_and(|at| tokio::time::Instant::now() < at)
    }
    pub(crate) async fn run(&self, cancel: CancellationToken) {
        use futures_util::{StreamExt, stream};
        let worker = || async {
            loop {
                let step = self.drive_one().await;
                let pause = match step {
                    Ok(true) => false,
                    Ok(false) => true,
                    Err(error) => {
                        tracing::warn!(%error, "canonical observation recovery remains pending");
                        true
                    }
                };
                if pause {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                } else {
                    tokio::task::yield_now().await;
                }
            }
        };
        tokio::select! {
            _ = cancel.cancelled() => {},
            _ = stream::iter(0..OBSERVATION_CONCURRENCY)
                .for_each_concurrent(OBSERVATION_CONCURRENCY, |_| worker()) => {},
        }
    }
    pub(crate) async fn drive_one(&self) -> Result<bool, String> {
        let Some(permit) = ObservationFailureBudget::acquire(&self.failure_budget) else {
            return Ok(false);
        };
        let result = self.drive_one_admitted().await;
        permit.finish(match &result {
            Ok((_, health)) => *health,
            Err(_) => Some(false),
        });
        result.map(|(progress, _)| progress)
    }
    async fn drive_one_admitted(&self) -> Result<(bool, Option<bool>), String> {
        let key = if let Some(key) = &self.scope {
            key.clone()
        } else {
            let mut pending = self.pending.lock().await;
            if pending.keys.is_empty() {
                if pending
                    .resume_at
                    .is_some_and(|until| tokio::time::Instant::now() < until)
                {
                    return Ok((false, None));
                }
                // Set before the await as well: a failed discovery must not let
                // eight workers retry the same unavailable dependency in a burst.
                pending.resume_at = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                pending.keys = tokio::time::timeout(Duration::from_secs(5), self.store.due_keys())
                    .await
                    .map_err(|_| "observation due discovery timed out")??
                    .into();
                if !pending.keys.is_empty() {
                    pending.resume_at = None;
                }
            }
            let Some(key) = pending.keys.pop_front() else {
                return Ok((false, None));
            };
            key
        };
        let candidate =
            tokio::time::timeout(Duration::from_secs(5), self.store.claim(&key.0, &key.1))
                .await
                .map_err(|_| "observation claim timed out")??;
        let Some(mut candidate) = candidate else {
            // Another owner already took this Session; continue to other due
            // identities instead of sleeping or discarding a whole shard.
            return Ok((self.scope.is_none(), None));
        };
        let result = tokio::time::timeout(Duration::from_secs(60), async {
            if !self
                .state
                .turn_persistence
                .observer_worker
                .admits_observation(&candidate.key.owner_user_id, &candidate.key.session_id)
                .await?
            {
                crate::server::run::lifecycle::record_turn_observer_run_metrics(
                    Some(&self.state.metrics_registry),
                    "canonical",
                    "access_disabled",
                );
                return Ok(());
            }
            let materialized = self.store.materialize(&candidate).await?;
            let same_generation = candidate
                .checkpoint
                .observed_head
                .as_ref()
                .is_some_and(|head| {
                    head.cursor.compaction_generation
                        == materialized.head.cursor.compaction_generation
                });
            let (messages, memory_operation_segment) =
                astra_turn_core::observer::filter_memory_operation_turns_with_context(
                    &materialized.prior_context,
                    &materialized.messages,
                    same_generation && candidate.checkpoint.memory_operation_segment,
                );
            candidate.next_memory_operation_segment = Some(memory_operation_segment);
            let messages = messages
                .into_iter()
                .filter_map(|message| message.as_object().cloned())
                .collect();
            crate::server::run::lifecycle::run_server_loop_observer_request(
                self.state.turn_persistence.observer_worker.as_ref(),
                &candidate.key.session_id,
                astra_turn_core::contracts::TurnObserverRequest {
                    user_id: candidate.key.owner_user_id.clone(),
                    session_id: candidate.key.session_id.clone(),
                    messages,
                    turn_count: i64::from(candidate.turn),
                    session_start: None,
                },
                "canonical",
                Some(&self.state.metrics_registry),
            )
            .await
        })
        .await
        .unwrap_or_else(|_| Err("canonical observation attempt timed out".into()));
        tokio::time::timeout(
            Duration::from_secs(5),
            self.store.finish(&candidate, result.is_ok()),
        )
        .await
        .map_err(|_| "observation checkpoint receipt timed out")??;
        let succeeded = result.is_ok();
        if let Err(error) = result {
            tracing::warn!(%error, "canonical observation attempt deferred");
        }
        Ok((true, Some(succeeded)))
    }
}

#[cfg(test)]
mod observation_failure_budget_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn shared_failure_budget_bounds_burst_backoff_and_cancelled_probe() {
        let budget = Arc::new(std::sync::Mutex::new(ObservationFailureBudget::default()));
        let held = ObservationFailureBudget::acquire(&budget).unwrap();
        for _ in 0..7 {
            ObservationFailureBudget::acquire(&budget)
                .unwrap()
                .finish(Some(false));
        }
        // A fast failing worker cannot refill the budget while another attempt
        // is held. Cancellation frees that reservation without claiming health.
        assert!(ObservationFailureBudget::acquire(&budget).is_none());
        drop(held);
        ObservationFailureBudget::acquire(&budget)
            .unwrap()
            .finish(Some(false));
        for _ in 0..1000 {
            assert!(ObservationFailureBudget::acquire(&budget).is_none());
        }
        tokio::time::advance(Duration::from_secs(5)).await;
        let probe = ObservationFailureBudget::acquire(&budget).unwrap();
        assert!(ObservationFailureBudget::acquire(&budget).is_none());
        probe.finish(Some(false));
        assert_eq!(budget.lock().unwrap().delay_secs, 10);
        tokio::time::advance(Duration::from_secs(10)).await;
        drop(ObservationFailureBudget::acquire(&budget).unwrap());
        assert!(ObservationFailureBudget::acquire(&budget).is_none());
        tokio::time::advance(Duration::from_secs(10)).await;
        ObservationFailureBudget::acquire(&budget)
            .unwrap()
            .finish(Some(true));
        for _ in 0..8 {
            ObservationFailureBudget::acquire(&budget)
                .unwrap()
                .finish(Some(true));
        }
        // Repeated failed probes reach a fixed cap, without a per-Session timer.
        for _ in 0..8 {
            ObservationFailureBudget::acquire(&budget)
                .unwrap()
                .finish(Some(false));
        }
        for _ in 0..20 {
            let delay = budget.lock().unwrap().delay_secs;
            tokio::time::advance(Duration::from_secs(delay)).await;
            ObservationFailureBudget::acquire(&budget)
                .unwrap()
                .finish(Some(false));
        }
        assert_eq!(budget.lock().unwrap().delay_secs, 960);
    }
}
