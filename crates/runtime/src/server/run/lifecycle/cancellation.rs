//! Session cancellation converges existing execution authority. A terminal run
//! label, or an empty active-run page, is not proof that its executor stopped.

use super::*;
use astra_services::SessionContextCoordinator;
use astra_services::runs::CancelSessionRecord;
use astra_services::session_context_coordinator::SessionExecutionBlocker;

impl AgenticRunLifecycleService {
    pub(super) async fn converge_session_cancellation(
        &self,
        session_id: &str,
        user_id: &str,
        request: bool,
    ) -> Result<CancelSessionRecord, (StatusCode, Json<ErrorResponse>)> {
        if !request {
            return match tokio::time::timeout(
                Duration::from_secs(1),
                self.observe_session_cancellation(session_id, user_id),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Ok(CancelSessionRecord {
                    runs: Vec::new(),
                    execution_settled: false,
                    execution_blocker: Some(SessionExecutionBlocker::OwnerUnavailable),
                }),
            };
        }
        self.run_engine
            .request_session_cancellation(user_id, session_id)
            .await
            .map_err(|error| Self::durable_persist_error("cancel session intent", error))?;
        let help = async {
            let mut observed = Vec::new();
            // The exact blocker is acted on before unrelated discovery. A slow
            // scan cannot consume its recovery opportunity on every request.
            if let Some((run_id, _)) = self
                .session_cancellation_primary_target(session_id, user_id)
                .await?
            {
                observed.push(
                    self.command_session_cancellation_target(session_id, user_id, &run_id)
                        .await?,
                );
            }
            for run_id in self
                .session_cancellation_candidates(session_id, user_id)
                .await?
            {
                if observed.iter().any(|run| run.run_id == run_id) {
                    continue;
                }
                observed.push(
                    self.command_session_cancellation_target(session_id, user_id, &run_id)
                        .await?,
                );
            }
            let execution_blocker = self
                .session_cancellation_blocker(session_id, user_id)
                .await?;
            Ok(CancelSessionRecord {
                execution_settled: observed.iter().all(|run| run.execution_settled)
                    && execution_blocker.is_none(),
                runs: observed,
                execution_blocker,
            })
        };
        match tokio::time::timeout(Duration::from_secs(1), help).await {
            Ok(result) => result,
            // Observe independently of the interrupted best-effort discovery.
            // In particular, a now-idle Session must not remain pending solely
            // because its history scan is slow. The status writer rechecks the
            // proof under its own Session transaction before publishing success.
            Err(_) => match tokio::time::timeout(
                Duration::from_secs(1),
                self.observe_session_cancellation(session_id, user_id),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Ok(CancelSessionRecord {
                    runs: Vec::new(),
                    execution_settled: false,
                    execution_blocker: Some(SessionExecutionBlocker::OwnerUnavailable),
                }),
            },
        }
    }

    async fn session_cancellation_primary_target(
        &self,
        session_id: &str,
        user_id: &str,
    ) -> Result<Option<(String, bool)>, (StatusCode, Json<ErrorResponse>)> {
        if let Some(run) = self
            .run_engine
            .find_blocking_session_run(user_id, session_id)
            .await
            .map_err(|error| Self::durable_persist_error("cancel blocking owner", error))?
        {
            return Ok(Some((run.run_id, false)));
        }
        if let Some(pool) = self.shared_pool.as_ref() {
            let prepared: Option<String> = sqlx::query_scalar(
                "SELECT run_id FROM tool_invocation_ledger WHERE user_id = ? AND session_id = ? AND state = 'prepared' LIMIT 1")
                .bind(user_id).bind(session_id).fetch_optional(pool.get()).await
                .map_err(|error| Self::durable_persist_error("observe terminal custody", error.to_string()))?;
            return Ok(prepared.map(|run_id| (run_id, true)));
        }
        Ok(None)
    }

    async fn observe_session_cancellation(
        &self,
        session_id: &str,
        user_id: &str,
    ) -> Result<CancelSessionRecord, (StatusCode, Json<ErrorResponse>)> {
        let mut observed = Vec::new();
        if let Some((run_id, prepared)) = self
            .session_cancellation_primary_target(session_id, user_id)
            .await?
        {
            if prepared {
                return Err(error_response_coded(
                    StatusCode::CONFLICT,
                    "Cancellation requires terminal tool reconciliation; repeat the cancellation command",
                    "cancellation_recovery_required",
                ));
            }
            let run = self
                .get_run_execution_settlement(run_id, user_id.to_owned())
                .await?;
            if !run.execution_settled {
                return Ok(CancelSessionRecord {
                    runs: vec![run],
                    execution_settled: false,
                    execution_blocker: Some(SessionExecutionBlocker::ActiveRun),
                });
            }
            observed.push(run);
        }
        let execution_blocker = self
            .session_cancellation_blocker(session_id, user_id)
            .await?;
        Ok(CancelSessionRecord {
            runs: observed,
            execution_settled: execution_blocker.is_none(),
            execution_blocker,
        })
    }

    async fn command_session_cancellation_target(
        &self,
        session_id: &str,
        user_id: &str,
        run_id: &str,
    ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
        let result = self
            .cancel_session_target(session_id, user_id, run_id)
            .await?;
        if durable_run_status_is_terminal(&result.status)
            && let Some(pool) = self.shared_pool.as_ref()
        {
            astra_services::tool_invocation_ledger::DatabaseToolInvocationLedger::new(pool.clone())
                .reconcile_terminal_run(user_id, session_id, run_id)
                .await
                .map_err(|error| {
                    Self::durable_persist_error("cancel tool reconciliation", error.to_string())
                })?;
        }
        Ok(result)
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
                self.cancel_run(run_id.to_owned(), user_id.to_owned()).await
            }
            // Discovery is only a hint; an unavailable or moved Run is never
            // proof that all Session execution and tool custody settled.
            _ => Ok(CancelRunRecord {
                run_id: run_id.to_owned(),
                status: "cancellation_requested".to_owned(),
                execution_settled: false,
                context_not_saved: None,
            }),
        }
    }

    async fn session_cancellation_candidates(
        &self,
        session_id: &str,
        user_id: &str,
    ) -> Result<BTreeSet<String>, (StatusCode, Json<ErrorResponse>)> {
        let mut candidates = BTreeSet::new();
        candidates.extend(
            self.runs
                .read()
                .await
                .values()
                .filter(|run| {
                    run.user_id == user_id && run.session_id == session_id && run.execution_live
                })
                .map(|run| run.run_id.clone()),
        );

        if let Some(pool) = self.shared_pool.as_ref() {
            // Discovery only; cancel_run rechecks owner/generation and the
            // final session fence proves quiescence. Include terminal leases
            // and blocking Runs or ledger owners even on a fresh request to another pod.
            let ids: Vec<String> = sqlx::query_scalar(
                "SELECT run_id FROM agent_runs WHERE user_id = ? AND session_id = ?
                   AND (status IN ('running', 'waiting', 'paused')
                        OR (owner_pod_id IS NOT NULL AND owner_lease_expires_at >= NOW(6)))
                 UNION SELECT run_id FROM tool_invocation_ledger WHERE user_id = ? AND session_id = ?
                   AND state IN ('prepared', 'dispatched', 'outcome_unknown')
                 LIMIT 100
",
            ).bind(user_id).bind(session_id).bind(user_id).bind(session_id)
                .fetch_all(pool.get()).await
                .map_err(|error| Self::durable_persist_error("cancel retained execution discovery", error.to_string()))?;
            candidates.extend(ids);
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
                    if matches!(
                        run.status.as_str(),
                        STATUS_RUNNING | STATUS_WAITING | STATUS_PAUSED
                    ) {
                        candidates.insert(run.run_id);
                        continue;
                    }
                    if let Some(control) = self
                        .run_engine
                        .load_run_control(user_id, &run.run_id)
                        .await
                        .map_err(|error| {
                            Self::durable_persist_error("cancel retained run control", error)
                        })?
                        && control.owner_lease_live
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

    async fn session_cancellation_blocker(
        &self,
        session_id: &str,
        user_id: &str,
    ) -> Result<Option<SessionExecutionBlocker>, (StatusCode, Json<ErrorResponse>)> {
        if let Some(pool) = self.shared_pool.as_ref() {
            let blocker = astra_services::DatabaseSessionContextCoordinator::new(pool.clone())
                .execution_reuse_blocker(&session_cancellation_key(user_id, session_id))
                .await
                .map_err(|error| {
                    Self::durable_persist_error("cancel convergence evidence", error.to_string())
                })?;
            return Ok(blocker.or(
                if self.runs.read().await.values().any(|run| {
                    run.user_id == user_id && run.session_id == session_id && run.execution_live
                }) {
                    Some(SessionExecutionBlocker::ActiveRun)
                } else {
                    None
                },
            ));
        }
        Ok((!self
            .session_cancellation_candidates(session_id, user_id)
            .await?
            .is_empty())
        .then_some(SessionExecutionBlocker::ActiveRun))
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
