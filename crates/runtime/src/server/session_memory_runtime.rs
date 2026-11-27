//! Recovery driver for canonical extraction and session-end governance.
use crate::turn::cloud::memoria_compact::UserScopedMemoriaPort;
use crate::{AppState, session_memory::durable::DurableMemoriaPort};
use astra_services::session_memory_lifecycle::SessionMemoryLifecycleStore;
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

const ACTIVE_RECOVERY_INTERVAL: Duration = Duration::from_secs(2);
const IDLE_RECOVERY_INTERVAL: Duration = Duration::from_secs(30);

pub(crate) fn spawn_session_memory_recovery(
    state: AppState,
    cancel: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    let pool = state.shared_pool.clone()?;
    state.auth_service.memoria_credentials()?;
    let store = SessionMemoryLifecycleStore::new(pool);
    Some(tokio::spawn(async move {
        while !cancel.is_cancelled() {
            let scan_started = tokio::time::Instant::now();
            let earliest_scan = scan_started + ACTIVE_RECOVERY_INTERVAL;
            let idle_deadline = scan_started + IDLE_RECOVERY_INTERVAL;
            let (due, next_scan) = match store
                .scan_recovery(16, IDLE_RECOVERY_INTERVAL.as_secs())
                .await
            {
                Ok(scan) if scan.due.is_empty() => {
                    let next_scan = scan.retry_after.map_or(idle_deadline, |delay| {
                        (tokio::time::Instant::now() + delay).min(idle_deadline)
                    });
                    (scan.due, next_scan)
                }
                Ok(scan) => (scan.due, earliest_scan),
                Err(error) => {
                    tracing::warn!(%error, "session memory recovery scan failed");
                    (Vec::new(), earliest_scan)
                }
            };
            use futures_util::{StreamExt, stream};
            let state = &state;
            let store = &store;
            let cancel = &cancel;
            stream::iter(due)
                .for_each_concurrent(2, |(user, session)| async move {
                    tokio::select! {
                        _ = cancel.cancelled() => {},
                        result = drive_session_memory_work(state, store, &user, &session) => {
                            if let Err(error) = result {
                                tracing::warn!(session_id = session, %error, "session memory recovery remains pending");
                            }
                        }
                    }
                })
                .await;
            if !wait_for_recovery_scan(
                &state.session_memory_wakeup,
                cancel,
                earliest_scan,
                next_scan,
            )
            .await
            {
                break;
            }
        }
    }))
}

async fn wait_for_recovery_scan(
    wakeup: &tokio::sync::Notify,
    cancel: &CancellationToken,
    earliest_scan: tokio::time::Instant,
    next_scan: tokio::time::Instant,
) -> bool {
    // A local commit may advance the deadline to the existing scan floor.
    tokio::select! {
        biased;
        _ = cancel.cancelled() => false,
        _ = async {
            tokio::select! {
                _ = wakeup.notified() => {},
                _ = tokio::time::sleep_until(next_scan) => {},
            }
            tokio::time::sleep_until(earliest_scan).await;
        } => true,
    }
}

pub(crate) async fn drive_session_memory_work(
    state: &AppState,
    store: &SessionMemoryLifecycleStore,
    user: &str,
    session: &str,
) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn memory_recovery_bounds_local_hints_and_keeps_retry_deadlines() {
        use tokio::time::Instant;

        let wakeup = tokio::sync::Notify::new();
        let cancel = CancellationToken::new();

        let start = Instant::now();
        for _ in 0..3 {
            let scan = Instant::now();
            wakeup.notify_one();
            assert!(
                wait_for_recovery_scan(
                    &wakeup,
                    &cancel,
                    scan + ACTIVE_RECOVERY_INTERVAL,
                    scan + IDLE_RECOVERY_INTERVAL,
                )
                .await
            );
        }
        assert_eq!(start.elapsed(), ACTIVE_RECOVERY_INTERVAL * 3);

        for delay in [
            ACTIVE_RECOVERY_INTERVAL,
            Duration::from_secs(5),
            IDLE_RECOVERY_INTERVAL,
        ] {
            let scan = Instant::now();
            assert!(
                wait_for_recovery_scan(
                    &wakeup,
                    &cancel,
                    scan + ACTIVE_RECOVERY_INTERVAL,
                    scan + delay
                )
                .await
            );
            assert_eq!(scan.elapsed(), delay);
        }

        cancel.cancel();
        let scan = Instant::now();
        assert!(
            !wait_for_recovery_scan(
                &wakeup,
                &cancel,
                scan + ACTIVE_RECOVERY_INTERVAL,
                scan + IDLE_RECOVERY_INTERVAL,
            )
            .await
        );
    }
}
