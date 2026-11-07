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
        let mut client = UserScopedMemoriaPort::new(resolver, user.into());
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
                .scoped_to_owner(user)?
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

/// Parallel recovery over bounded canonical identity windows. Each process owns
/// at most eight shard batches and eight transcript/HTTP attempts. Thirty-two
/// short-lived shard leases allow independent replicas to share the backlog.
pub(crate) struct SessionObservationRecovery {
    state: AppState,
    store: astra_services::session_observation::SessionObservationStore,
    lease: super::sweeper_lease::SweeperLease,
    scope: Option<(String, String)>,
    scan: tokio::sync::Mutex<ObservationScan>,
    retry: Arc<std::sync::Mutex<std::collections::VecDeque<u64>>>,
    active_shards: Arc<std::sync::Mutex<std::collections::HashSet<u64>>>,
    capacity: Arc<tokio::sync::Semaphore>,
}

const OBSERVATION_CONCURRENCY: usize = 8;

#[derive(Default)]
struct ObservationScan {
    next: u64,
    horizon: Option<u64>,
    resume_at: Option<tokio::time::Instant>,
}

struct ObservationWindow {
    after: u64,
    retry: Arc<std::sync::Mutex<std::collections::VecDeque<u64>>>,
    shards: Arc<std::sync::Mutex<std::collections::HashSet<u64>>>,
    complete: bool,
}
impl Drop for ObservationWindow {
    fn drop(&mut self) {
        self.shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&((self.after / 64) % 32));
        if !self.complete {
            self.retry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_back(self.after);
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
            scope: None,
            scan: Default::default(),
            retry: Default::default(),
            active_shards: Default::default(),
            capacity: Arc::new(tokio::sync::Semaphore::new(OBSERVATION_CONCURRENCY)),
            store: astra_services::session_observation::SessionObservationStore::new(pool.clone()),
            lease: super::sweeper_lease::SweeperLease::new(
                pool,
                uuid::Uuid::new_v4().to_string(),
                "session_observation_test".into(),
                "sweeper_leases".into(),
            ),
        })
    }

    #[cfg(any(test, feature = "e2e-hooks"))]
    pub(crate) fn scope_for_test(&mut self, user: &str, session: &str) {
        self.scope = Some((user.into(), session.into()));
        self.lease.lease_name = format!("session_observation_test_{session}");
    }

    #[cfg(any(test, feature = "e2e-hooks"))]
    pub(crate) fn lease_identity_for_test(&self) -> (&str, &str) {
        (&self.lease.lease_name, &self.lease.pod_id)
    }

    #[cfg(any(test, feature = "e2e-hooks"))]
    pub(crate) async fn expire_lease_for_test(&self) {
        sqlx::query("UPDATE sweeper_leases SET expires_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND) WHERE owner_pod_id = ?")
            .bind(&self.lease.pod_id).execute(self.lease.pool.get()).await.unwrap();
    }

    pub(crate) async fn prepare(&self) -> bool {
        if self.scope.is_none() {
            return true;
        }
        matches!(
            self.lease.check_leader().await,
            super::sweeper_lease::LeaderStatus::Leader
        )
    }

    /// This is the production timer/dispatch loop, also exercised by integration
    /// tests. Busy/slow Sessions occupy bounded slots, not a deployment-wide lock.
    pub(crate) async fn run(&self, cancel: CancellationToken) {
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
            _ = async {
                use futures_util::{StreamExt, stream};
                // A held tail in one window must not leave otherwise available
                // attempt slots idle. Window concurrency matches the attempt cap.
                stream::iter(0..OBSERVATION_CONCURRENCY)
                    .for_each_concurrent(OBSERVATION_CONCURRENCY, |_| worker()).await;
            } => {},
        }
    }

    pub(crate) async fn drive_one(&self) -> Result<bool, String> {
        if let Some(key) = &self.scope {
            if !self.prepare().await {
                return Ok(false);
            }
            return self
                .with_lease_renewal(&self.lease, self.attempt(&self.lease, key))
                .await;
        }
        let retry = self
            .retry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front();
        let after = if let Some(after) = retry {
            after
        } else {
            let mut scan = self.scan.lock().await;
            // The end-of-sweep pause belongs to the shared cursor. Otherwise
            // another window worker could immediately start the next idle
            // sweep while only the worker that reached the end sleeps.
            if scan
                .resume_at
                .is_some_and(|until| tokio::time::Instant::now() < until)
            {
                return Ok(false);
            }
            scan.resume_at = None;
            if scan.horizon.is_none() {
                scan.horizon = Some(
                    tokio::time::timeout(Duration::from_secs(5), self.store.catalog_high_water())
                        .await
                        .map_err(|_| "observation catalog bound timed out")??,
                );
                scan.next = 0;
            }
            if scan.next >= scan.horizon.unwrap_or(0) {
                scan.horizon = None;
                scan.resume_at = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                return Ok(false);
            }
            let after = scan.next;
            scan.next = scan.next.saturating_add(64);
            after
        };
        let shard = (after / 64) % 32;
        if !self
            .active_shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(shard)
        {
            // Preserve a failed window even if a different window in the same
            // shard is still active. New windows remain visible next sweep.
            if retry.is_some() {
                self.retry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push_back(after);
                return Ok(false);
            }
            return Ok(true);
        }
        let mut window = ObservationWindow {
            after,
            retry: self.retry.clone(),
            shards: self.active_shards.clone(),
            complete: false,
        };
        let lease = super::sweeper_lease::SweeperLease::new(
            self.lease.pool.clone(),
            self.lease.pod_id.clone(),
            format!("session_observation_v2_{shard}"),
            "sweeper_leases".into(),
        );
        match lease.check_leader().await {
            super::sweeper_lease::LeaderStatus::Leader => {}
            super::sweeper_lease::LeaderStatus::NotLeader => {
                window.complete = true;
                return Ok(true);
            }
            super::sweeper_lease::LeaderStatus::Unavailable(_) => {
                return Err("observation lease unavailable".into());
            }
        }
        let work = async {
            use futures_util::{StreamExt, stream};
            let keys = tokio::time::timeout(Duration::from_secs(5), self.store.key_page(after))
                .await
                .map_err(|_| "observation key page timed out")??;
            let keys = tokio::time::timeout(
                Duration::from_secs(5),
                self.store.pending_keys(after, &keys),
            )
            .await
            .map_err(|_| "observation eligibility timed out")??;
            let results = stream::iter(keys)
                .map(|key| {
                    let lease = &lease;
                    async move {
                        let _permit = self
                            .capacity
                            .acquire()
                            .await
                            .map_err(|_| "observation owner stopped")?;
                        self.attempt(lease, &key).await
                    }
                })
                .buffer_unordered(OBSERVATION_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
            for result in results {
                result?;
            }
            Ok(true)
        };
        let result = self.with_lease_renewal(&lease, work).await;
        // Do not hold idle shards for a whole TTL: another replica can claim
        // them immediately. Failure/cancellation retains this numeric window.
        tokio::time::timeout(
            Duration::from_secs(5),
            self.store.release_lease(&lease.lease_name, &lease.pod_id),
        )
        .await
        .map_err(|_| "observation lease release timed out")??;
        window.complete = result.is_ok();
        result
    }

    async fn with_lease_renewal<T>(
        &self,
        lease: &super::sweeper_lease::SweeperLease,
        work: impl std::future::Future<Output = Result<T, String>>,
    ) -> Result<T, String> {
        tokio::pin!(work);
        let mut refresh = tokio::time::interval(Duration::from_secs(10));
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                result = &mut work => return result,
                _ = refresh.tick() => {
                    if !matches!(lease.check_leader().await, super::sweeper_lease::LeaderStatus::Leader) {
                        return Err("observation lease lost during batch".into());
                    }
                }
            }
        }
    }

    async fn attempt(
        &self,
        lease: &super::sweeper_lease::SweeperLease,
        key: &(String, String),
    ) -> Result<bool, String> {
        let candidate = tokio::time::timeout(
            Duration::from_secs(5),
            self.store
                .next(&lease.lease_name, &lease.pod_id, &key.0, &key.1),
        )
        .await
        .map_err(|_| "observation discovery timed out")??;
        let Some(candidate) = candidate else {
            return Ok(false);
        };
        let result = tokio::time::timeout(Duration::from_secs(60), async {
            if !self
                .state
                .turn_persistence
                .observer_worker
                .admits_observation(&candidate.key.owner_user_id)
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
            let messages =
                astra_turn_core::observer::filter_memory_operation_turns(&materialized.messages)
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
            self.store
                .finish(&lease.lease_name, &lease.pod_id, &candidate, result.is_ok()),
        )
        .await
        .map_err(|_| "observation checkpoint receipt timed out")??;
        if let Err(error) = result {
            tracing::warn!(%error, "canonical observation attempt deferred");
        }
        Ok(true)
    }
}
