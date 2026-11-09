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

/// One bounded due-page reader per process feeds eight independent Session
/// attempts. Shared idle cooldown makes empty polling one read per two seconds,
/// not one read per worker. The durable per-Session due marker owns crash retry.
pub(crate) struct SessionObservationRecovery {
    state: AppState,
    store: astra_services::session_observation::SessionObservationStore,
    scope: Option<(String, String)>,
    pending: tokio::sync::Mutex<ObservationPending>,
}
const OBSERVATION_CONCURRENCY: usize = 8;
#[derive(Default)]
struct ObservationPending {
    keys: std::collections::VecDeque<(String, String)>,
    resume_at: Option<tokio::time::Instant>,
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
        let key = if let Some(key) = &self.scope {
            key.clone()
        } else {
            let mut pending = self.pending.lock().await;
            if pending.keys.is_empty() {
                if pending
                    .resume_at
                    .is_some_and(|until| tokio::time::Instant::now() < until)
                {
                    return Ok(false);
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
                return Ok(false);
            };
            key
        };
        let candidate =
            tokio::time::timeout(Duration::from_secs(5), self.store.claim(&key.0, &key.1))
                .await
                .map_err(|_| "observation claim timed out")??;
        let Some(candidate) = candidate else {
            // Another owner already took this Session; continue to other due
            // identities instead of sleeping or discarding a whole shard.
            return Ok(self.scope.is_none());
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
            self.store.finish(&candidate, result.is_ok()),
        )
        .await
        .map_err(|_| "observation checkpoint receipt timed out")??;
        if let Err(error) = result {
            tracing::warn!(%error, "canonical observation attempt deferred");
        }
        Ok(true)
    }
}
