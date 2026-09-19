//! Session cancellation converges existing execution authority. A terminal run
//! label, or an empty active-run page, is not proof that its executor stopped.

use super::*;
use astra_services::SessionContextCoordinator;
use astra_services::runs::CancelSessionRecord;
use astra_services::session_context_coordinator::WorkspaceReuseBlocker;

const SESSION_CANCEL_CONVERGENCE_BUDGET: Duration = Duration::from_secs(1);
const SESSION_CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(50);

impl AgenticRunLifecycleService {
    pub(super) async fn converge_session_cancellation(
        &self,
        session_id: &str,
        user_id: &str,
    ) -> Result<CancelSessionRecord, (StatusCode, Json<ErrorResponse>)> {
        let mut observed = BTreeMap::new();
        // Publish all intent in one bounded durable operation before doing any
        // settlement work. A slow/large discovery page cannot starve the tail.
        self.run_engine
            .request_session_cancellation(user_id, session_id)
            .await
            .map_err(|error| Self::durable_persist_error("cancel session intent", error))?;
        let deadline = tokio::time::Instant::now() + SESSION_CANCEL_CONVERGENCE_BUDGET;
        let mut workspace_blocker;
        let execution_settled = 'convergence: {
            loop {
                let mut candidates = self
                    .session_cancellation_candidates(session_id, user_id)
                    .await?;
                // Retain exact targets across iterations: terminal transitions
                // remove them from active discovery before execution retires.
                candidates.extend(observed.keys().cloned());
                let mut all_observed = true;
                for run_id in candidates {
                    if tokio::time::Instant::now() >= deadline && !observed.is_empty() {
                        all_observed = false;
                        break;
                    }
                    let result = self
                        .cancel_session_target(session_id, user_id, &run_id)
                        .await?;
                    if durable_run_status_is_terminal(&result.status)
                        && let Some(pool) = self.shared_pool.as_ref()
                    {
                        astra_services::tool_invocation_ledger::DatabaseToolInvocationLedger::new(
                            pool.clone(),
                        )
                        .reconcile_terminal_run(user_id, session_id, &run_id)
                        .await
                        .map_err(|error| {
                            Self::durable_persist_error(
                                "cancel tool reconciliation",
                                error.to_string(),
                            )
                        })?;
                    }
                    observed.insert(run_id, result);
                }
                self.release_cancelled_run_writer(session_id, user_id, &observed)
                    .await?;
                workspace_blocker = self
                    .session_cancellation_blocker(session_id, user_id)
                    .await?;
                if all_observed
                    && observed.values().all(|run| run.execution_settled)
                    && workspace_blocker.is_none()
                {
                    break 'convergence true;
                }
                if tokio::time::Instant::now() >= deadline {
                    break 'convergence false;
                }
                tokio::time::sleep(SESSION_CANCEL_POLL_INTERVAL).await;
            }
        };
        Ok(CancelSessionRecord {
            runs: observed.into_values().collect(),
            execution_settled,
            workspace_blocker,
        })
    }

    async fn cancel_session_target(
        &self,
        session_id: &str,
        user_id: &str,
        run_id: &str,
    ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
        match self
            .run_engine
            .load_run_control(user_id, run_id)
            .await
            .map_err(|error| Self::durable_persist_error("cancel target lookup", error))?
        {
            Some(control) if control.session_id == session_id => {
                let mut result = self
                    .cancel_run(run_id.to_owned(), user_id.to_owned())
                    .await?;
                if result.execution_settled
                    && self
                        .run_engine
                        .has_open_run_settlement(user_id, run_id, control.run_generation)
                        .await
                        .map_err(|error| {
                            Self::durable_persist_error("cancel settlement lookup", error)
                        })?
                {
                    result.execution_settled = false;
                }
                Ok(result)
            }
            // Writer acquisition can precede durable Run insertion. The
            // reservation is still authority; missing Run is never idle proof.
            _ => Ok(CancelRunRecord {
                run_id: run_id.to_owned(),
                status: "cancellation_requested".to_owned(),
                execution_settled: false,
            }),
        }
    }

    async fn session_cancellation_candidates(
        &self,
        session_id: &str,
        user_id: &str,
    ) -> Result<BTreeSet<String>, (StatusCode, Json<ErrorResponse>)> {
        let mut candidates = BTreeSet::new();
        let mut cursor = None;
        loop {
            let page = self
                .run_engine
                .list_active_session_runs_cursor(user_id, session_id, 100, cursor)
                .await
                .map_err(|error| Self::durable_persist_error("cancel session discovery", error))?;
            candidates.extend(page.runs.into_iter().map(|run| run.run_id));
            let Some(next) = page.next_cursor else {
                break;
            };
            cursor = Some(next);
        }
        candidates.extend(
            self.runs
                .read()
                .await
                .values()
                .filter(|run| {
                    run.user_id == user_id
                        && run.session_id == session_id
                        && (run.execution_live || run.settlement_in_progress)
                })
                .map(|run| run.run_id.clone()),
        );

        if let Some(pool) = self.shared_pool.as_ref() {
            // Discovery only; cancel_run rechecks owner/generation and the
            // final session fence proves quiescence. Include terminal leases
            // and ledger/slot owners even on a fresh request to another pod.
            let ids: Vec<String> = sqlx::query_scalar(
                "SELECT run_id FROM agent_runs WHERE user_id = ? AND session_id = ?
                   AND owner_pod_id IS NOT NULL AND owner_lease_expires_at >= NOW(6)
                 UNION SELECT run_id FROM tool_invocation_ledger WHERE user_id = ? AND session_id = ?
                   AND state IN ('prepared', 'dispatched', 'outcome_unknown')
                 UNION SELECT run_id FROM agent_session_execution_slots WHERE user_id = ? AND session_id = ?
                 UNION SELECT r.run_id FROM agent_runs r WHERE r.user_id = ? AND r.session_id = ?
                   AND EXISTS (SELECT 1 FROM agent_run_events e WHERE e.user_id = r.user_id AND e.run_id = r.run_id
                               AND e.idempotency_key = CONCAT('run-settlement-started:', r.run_generation))
                   AND NOT EXISTS (SELECT 1 FROM agent_run_events e WHERE e.user_id = r.user_id AND e.run_id = r.run_id
                                   AND e.idempotency_key IN (CONCAT('run-settlement-finished:', r.run_generation), CONCAT('run-accounting-finalized:', r.run_generation)))",
            ).bind(user_id).bind(session_id).bind(user_id).bind(session_id).bind(user_id).bind(session_id).bind(user_id).bind(session_id)
                .fetch_all(pool.get()).await
                .map_err(|error| Self::durable_persist_error("cancel retained execution discovery", error.to_string()))?;
            candidates.extend(ids);
            let coordinator = astra_services::DatabaseSessionContextCoordinator::new(pool.clone());
            if let Some(writer) = coordinator
                .load_active_writer(&session_cancellation_key(user_id, session_id))
                .await
                .map_err(|error| {
                    Self::durable_persist_error("cancel writer discovery", error.to_string())
                })?
                && let Some(run_id) = internal_writer_run_id(&writer)
            {
                candidates.insert(run_id.to_owned());
            }
        } else {
            // Process-local stores have no SQL discovery seam. Seek through
            // their durable owner records rather than a truncated session tree.
            let mut cursor = None;
            loop {
                let page = self
                    .run_engine
                    .list_user_runs_cursor(user_id, 100, cursor)
                    .await
                    .map_err(|error| {
                        Self::durable_persist_error("cancel retained run discovery", error)
                    })?;
                for run in page
                    .runs
                    .into_iter()
                    .filter(|run| run.session_id == session_id)
                {
                    if let Some(control) = self
                        .run_engine
                        .load_run_control(user_id, &run.run_id)
                        .await
                        .map_err(|error| {
                            Self::durable_persist_error("cancel retained run control", error)
                        })?
                        && (control.owner_lease_live
                            || self
                                .run_engine
                                .has_open_run_settlement(
                                    user_id,
                                    &run.run_id,
                                    control.run_generation,
                                )
                                .await
                                .map_err(|error| {
                                    Self::durable_persist_error("cancel retained settlement", error)
                                })?)
                    {
                        candidates.insert(run.run_id);
                    }
                }
                let Some(next) = page.next_cursor else {
                    break;
                };
                cursor = Some(next);
            }
        }
        Ok(candidates)
    }

    async fn release_cancelled_run_writer(
        &self,
        session_id: &str,
        user_id: &str,
        observed: &BTreeMap<String, CancelRunRecord>,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let Some(pool) = self.shared_pool.as_ref() else {
            return Ok(());
        };
        let coordinator = astra_services::DatabaseSessionContextCoordinator::new(pool.clone());
        let Some(writer) = coordinator
            .load_active_writer(&session_cancellation_key(user_id, session_id))
            .await
            .map_err(|error| {
                Self::durable_persist_error("cancel writer lookup", error.to_string())
            })?
        else {
            return Ok(());
        };
        let Some(run_id) = internal_writer_run_id(&writer) else {
            return Ok(());
        };
        if !observed
            .get(run_id)
            .is_some_and(|run| run.execution_settled && durable_run_status_is_terminal(&run.status))
        {
            return Ok(());
        }
        if self.runs.read().await.get(run_id).is_some_and(|run| {
            run.user_id == user_id && (run.execution_live || run.settlement_in_progress)
        }) {
            return Ok(());
        }
        // The ordinary coordinator release compares lease id + writer epoch
        // and clears its reservation atomically. Never release a controller's
        // externally supplied writer, or the current writer of a newer run.
        let Some(control) = self
            .run_engine
            .load_run_control(user_id, run_id)
            .await
            .map_err(|error| Self::durable_persist_error("cancel writer run lookup", error))?
        else {
            return Ok(());
        };
        coordinator
            .release_terminal_execution_writer(&writer, run_id, control.run_generation)
            .await
            .map_err(|error| {
                Self::durable_persist_error("cancel writer release", error.to_string())
            })?;
        Ok(())
    }

    async fn session_cancellation_blocker(
        &self,
        session_id: &str,
        user_id: &str,
    ) -> Result<Option<WorkspaceReuseBlocker>, (StatusCode, Json<ErrorResponse>)> {
        if let Some(pool) = self.shared_pool.as_ref() {
            let blocker = astra_services::DatabaseSessionContextCoordinator::new(pool.clone())
                .execution_reuse_blocker(&session_cancellation_key(user_id, session_id))
                .await
                .map_err(|error| {
                    Self::durable_persist_error("cancel convergence evidence", error.to_string())
                })?;
            return Ok(blocker.or(
                if self.runs.read().await.values().any(|run| {
                    run.user_id == user_id
                        && run.session_id == session_id
                        && (run.execution_live || run.settlement_in_progress)
                }) {
                    Some(WorkspaceReuseBlocker::ActiveRun)
                } else {
                    None
                },
            ));
        }
        Ok((!self
            .session_cancellation_candidates(session_id, user_id)
            .await?
            .is_empty())
        .then_some(WorkspaceReuseBlocker::ActiveRun))
    }
}

fn session_cancellation_key(user_id: &str, session_id: &str) -> astra_turn_types::SessionKeyV1 {
    astra_turn_types::SessionKeyV1::owner_session(
        "server",
        user_id,
        session_id,
        astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID,
    )
}

fn internal_writer_run_id(writer: &astra_turn_types::ConversationWriterLeaseV1) -> Option<&str> {
    if writer.actor.actor_kind != astra_turn_types::ActorKindV1::Server {
        return None;
    }
    let run_id = writer.actor.actor_id.strip_prefix("server-run:")?;
    (writer.idempotency_key == format!("server-run:{run_id}:writer") && !run_id.is_empty())
        .then_some(run_id)
}
