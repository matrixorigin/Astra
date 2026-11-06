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
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { _ = cancel.cancelled() => break, _ = interval.tick() => {} }
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
                let cancel = cancel.clone();
                async move {
                    tokio::select! {
                        _ = cancel.cancelled() => {},
                        result = drive_session_memory_work(&state, &user, &session) => {
                            if let Err(error) = result { tracing::warn!(session_id = session, %error, "session memory recovery remains pending"); }
                        }
                    }
                }
            }).await;
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
        if let Some(observation) = &claim.work.active_observation {
            let request: astra_turn_core::contracts::TurnObserverRequest =
                serde_json::from_value(observation.request.clone())
                    .map_err(|_| "invalid durable observation payload")?;
            if request.user_id != user
                || request.session_id != session
                || request.turn_count != observation.turn
            {
                return Err("durable observation ownership mismatch".into());
            }
            crate::server::run::lifecycle::run_server_loop_observer_request(
                state.turn_persistence.observer_worker.as_ref(),
                session,
                request,
                "durable",
                Some(&state.metrics_registry),
            )
            .await?;
            return store.finish_observation(&claim).await;
        }
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

#[cfg(test)]
mod observation_tests {
    use super::*;
    use crate::ServiceInfo;
    use astra_core::{MatrixOneSettings, SharedPool};
    use astra_services::session_memory_lifecycle::{
        SessionObservationWork, enqueue_observation_tx,
    };
    use astra_turn_core::contracts::{
        TurnObserverOutcome, TurnObserverRequest, TurnObserverWorker,
    };
    use sqlx::Connection;
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    struct Healthy;
    #[async_trait::async_trait]
    impl crate::HealthChecker for Healthy {
        async fn database_healthy(&self) -> bool {
            true
        }
    }
    struct HeldObserver {
        calls: Mutex<std::collections::BTreeSet<String>>,
        entered: AtomicUsize,
        fail_next: AtomicBool,
        release: tokio::sync::Semaphore,
    }
    #[async_trait::async_trait]
    impl TurnObserverWorker for HeldObserver {
        async fn run(&self, request: TurnObserverRequest) -> Result<TurnObserverOutcome, String> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            if self.fail_next.swap(false, Ordering::SeqCst) {
                return Err("synthetic transient authority timeout".into());
            }
            self.release.acquire().await.unwrap().forget();
            assert!(self.calls.lock().unwrap().insert(request.session_id));
            Ok(TurnObserverOutcome::Observed)
        }
    }
    async fn publish(pool: &SharedPool, user: &str, session: &str) {
        sqlx::query(
            "INSERT INTO agent_sessions (user_id, session_id, status) VALUES (?, ?, 'active')",
        )
        .bind(user)
        .bind(session)
        .execute(pool.get())
        .await
        .unwrap();
        let mut connection = astra_services::CancellationSafePoolConnection::acquire(pool.get())
            .await
            .unwrap();
        let mut tx = connection.connection_mut().begin().await.unwrap();
        astra_services::storage::admit_session_event_write(&mut tx, session, user, false)
            .await
            .unwrap();
        enqueue_observation_tx(&mut tx, user, session, SessionObservationWork {
            event_id:session.to_owned(), turn:1,
            request:serde_json::to_value(TurnObserverRequest {user_id:user.into(), session_id:session.into(),
                messages:vec![serde_json::from_value(serde_json::json!({"role":"user","content":"synthetic durable observation"})).unwrap()], turn_count:1, session_start:None}).unwrap(),
        }).await.unwrap();
        tx.commit().await.unwrap();
        connection.release();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires isolated ASTRA_TEST_DATABASE and ASTRA_TEST_DB_IT=1"]
    async fn durable_observation_burst_drains_after_worker_saturation_and_restart() {
        use futures_util::{StreamExt, stream};
        let mut settings = MatrixOneSettings::from_env();
        crate::isolated_database::require_isolated_database(&settings.database);
        let mut admin_settings = settings.clone();
        admin_settings.database = "mysql".into();
        let mut admin =
            sqlx::MySqlConnection::connect(&admin_settings.database_url_with_password())
                .await
                .unwrap();
        settings.database = format!("astra_test_observer_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE DATABASE `{}`",
            settings.database
        )))
        .execute(&mut admin)
        .await
        .unwrap();
        astra_services::storage::ensure_core_schema(&settings, "mysql")
            .await
            .unwrap();
        let pool = SharedPool::new(&settings).await.unwrap();
        let user = format!("observer-burst-{}", uuid::Uuid::new_v4().simple());
        let sessions: Vec<_> = (0..1000).map(|n| format!("{user}-{n}")).collect();
        let worker = Arc::new(HeldObserver {
            calls: Default::default(),
            entered: AtomicUsize::new(0),
            fail_next: AtomicBool::new(false),
            release: tokio::sync::Semaphore::new(0),
        });
        let state = AppState::new(ServiceInfo::default(), Arc::new(Healthy))
            .with_shared_pool(pool.clone())
            .with_turn_observer_worker(worker.clone());
        stream::iter(&sessions)
            .for_each_concurrent(4, |session| publish(&pool, &user, session))
            .await;
        let first: Vec<_> = sessions
            .iter()
            .take(2)
            .map(|session| {
                let state = state.clone();
                let user = user.clone();
                let session = session.clone();
                tokio::spawn(async move {
                    drive_session_memory_work(&state, &user, &session)
                        .await
                        .unwrap()
                })
            })
            .collect();
        tokio::time::timeout(Duration::from_secs(10), async {
            while worker.entered.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Every bounded worker is occupied. The other independent Sessions
        // remain durable, with no detached tasks and no dropped_full result.
        let pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_session_memory_work WHERE user_id = ? AND complete = 0",
        )
        .bind(&user)
        .fetch_one(pool.get())
        .await
        .unwrap();
        assert_eq!(pending, 1000);
        worker.release.add_permits(1000);
        for task in first {
            task.await.unwrap();
        }
        // Construct a new store/driver view, as after restart. The real SQL
        // claims, checkpoints and bounded production work function own replay.
        let store = SessionMemoryLifecycleStore::new(pool.clone());
        worker.fail_next.store(true, Ordering::SeqCst);
        assert!(
            drive_session_memory_work(&state, &user, &sessions[2])
                .await
                .is_err()
        );
        assert!(
            store.status(&user, &sessions[2]).await.unwrap()["pending"]
                .as_bool()
                .unwrap()
        );
        assert!(
            store.claim(&user, &sessions[2]).await.unwrap().is_none(),
            "transient errors must back off"
        );
        sqlx::query("UPDATE agent_session_memory_work SET next_attempt_at = NOW(6) WHERE user_id = ? AND session_id = ?")
            .bind(&user).bind(&sessions[2]).execute(pool.get()).await.unwrap();
        // A pod dies after claiming but before HTTP dispatch. Another driver
        // must not take a live lease; expiry makes the exact payload replayable.
        let abandoned = store.claim(&user, &sessions[3]).await.unwrap().unwrap();
        assert!(store.claim(&user, &sessions[3]).await.unwrap().is_none());
        sqlx::query("UPDATE agent_session_memory_work SET lease_expires_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND) WHERE user_id = ? AND session_id = ? AND lease_token = ?")
            .bind(&user).bind(&sessions[3]).bind(&abandoned.token).execute(pool.get()).await.unwrap();
        assert!(
            store.finish_observation(&abandoned).await.is_err(),
            "an expired worker cannot acknowledge work"
        );
        loop {
            let due = store.list_due(16).await.unwrap();
            let due: Vec<_> = due
                .into_iter()
                .filter(|(owner, _)| owner == &user)
                .collect();
            if due.is_empty() {
                break;
            }
            stream::iter(due)
                .for_each_concurrent(2, |(owner, session)| {
                    let state = state.clone();
                    async move {
                        drive_session_memory_work(&state, &owner, &session)
                            .await
                            .unwrap();
                    }
                })
                .await;
        }
        {
            let seen = worker.calls.lock().unwrap();
            for session in &sessions {
                assert!(
                    seen.contains(session),
                    "lost committed observation: {session}"
                );
            }
            assert_eq!(seen.len(), sessions.len());
        }
        pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE `{}`",
            settings.database
        )))
        .execute(&mut admin)
        .await
        .unwrap();
    }
}
