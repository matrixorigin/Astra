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
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                for _ in 0..16 {
                    match observation.drive_one().await {
                        Ok(true) => {}
                        Ok(false) => break,
                        Err(error) => {
                            tracing::warn!(%error, "canonical observation recovery remains pending");
                            break;
                        }
                    }
                }
            }
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

/// The existing managed recovery owner reads committed canonical facts. A
/// bounded leader drains coalesced Sessions without a per-chat enqueue write.
pub(crate) struct SessionObservationRecovery {
    state: AppState,
    store: astra_services::session_observation::SessionObservationStore,
    lease: super::sweeper_lease::SweeperLease,
    scope: Option<(String, String)>,
    scan: tokio::sync::Mutex<ObservationScan>,
}

#[derive(Default)]
struct ObservationScan {
    after: Option<(String, String)>,
    keys: std::collections::VecDeque<(String, String)>,
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
            store: astra_services::session_observation::SessionObservationStore::new(pool.clone()),
            lease: super::sweeper_lease::SweeperLease::new(
                pool,
                uuid::Uuid::new_v4().to_string(),
                "session_observation".into(),
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
        sqlx::query("UPDATE sweeper_leases SET expires_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND) WHERE sweeper_name = ? AND owner_pod_id = ?")
            .bind(&self.lease.lease_name).bind(&self.lease.pod_id).execute(self.lease.pool.get()).await.unwrap();
    }

    pub(crate) async fn prepare(&self) -> bool {
        matches!(
            self.lease.check_leader().await,
            super::sweeper_lease::LeaderStatus::Leader
        )
    }

    pub(crate) async fn drive_one(&self) -> Result<bool, String> {
        let mut scan = self.scan.lock().await;
        if !self.prepare().await {
            return Ok(false);
        }
        let key = if let Some(scope) = &self.scope {
            scope.clone()
        } else {
            if scan.keys.is_empty() {
                let page = self
                    .store
                    .key_page(
                        scan.after
                            .as_ref()
                            .map(|(user, session)| (user.as_str(), session.as_str())),
                    )
                    .await?;
                if let Some(last) = page.last() {
                    let pending = self.store.pending_keys(&page).await?;
                    scan.after = Some(last.clone());
                    scan.keys = pending.into();
                    if scan.keys.is_empty() {
                        // One page was examined, even though no work was due.
                        return Ok(true);
                    }
                }
            }
            let Some(key) = scan.keys.front().cloned() else {
                scan.after = None;
                return Ok(false);
            };
            key
        };
        let Some(candidate) = self
            .store
            .next(&self.lease.lease_name, &self.lease.pod_id, &key.0, &key.1)
            .await?
        else {
            if self.scope.is_some() {
                return Ok(false);
            }
            // An idle/backed-off/deleted Session still consumes one bounded
            // scan step. Never reset to the start merely because it is idle.
            scan.keys.pop_front();
            return Ok(true);
        };
        // The cursor is only a scan optimization. Checkpoints and canonical
        // facts remain durable; restarting or wrapping can never lose work.
        // Advance only after checkpointing; cancellation/errors replay this key.
        let result = async {
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
        }
        .await;
        self.store
            .finish(
                &self.lease.lease_name,
                &self.lease.pod_id,
                &candidate,
                result.is_ok(),
            )
            .await?;
        if self.scope.is_none() {
            scan.keys.pop_front();
        }
        if let Err(error) = result {
            // The failed attempt has a durable retry clock. Continue the bounded
            // batch so a few failing Sessions cannot starve healthy Sessions.
            tracing::warn!(%error, "canonical observation attempt deferred");
        }
        Ok(true)
    }
}
