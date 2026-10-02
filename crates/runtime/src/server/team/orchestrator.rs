//! Team execution orchestrator — 4-phase pipeline bridging `/team run` to `DelegationEngine`.
//!
//! # Phases
//!
//! 1. **Prepare** — load team, validate, resolve profiles, create worktrees, start durable run
//! 2. **Execute** — dispatch through DelegationEngine with event logging
//! 3. **Merge** — merge worktrees, aggregate learnings
//! 4. **Report** — persist final status, record execution history, produce summary
//!
//! The orchestrator wraps existing infrastructure (DelegationEngine, RunEngine,
//! WorktreeManager) rather than replacing it.

use std::sync::Arc;

use tokio::sync::RwLock;

use astra_core::{STATUS_COMPLETED, STATUS_FAILED, STATUS_RUNNING};
use astra_services::coordination::{AgentProfile, AgentProfileRegistry, DelegationResult};
use astra_services::team_persistence::{TeamPersistenceService, WorktreeMode, resolve_team};

use astra_server_types::team_orchestrator_traits::{
    DelegationExecutor, DelegationTracking, RunPersistence,
};
pub use astra_server_types::team_orchestrator_types::{
    ExecutionPhase, OrchestratorConfig, ProgressCallback, TeamExecutionStatus,
    append_merge_conflict_summary, derive_team_status, sum_usage, summarize_unsuccessful_agents,
};
use astra_server_types::warn_persist;
use sha2::Digest;

use crate::server::conflict_resolver;
use astra_server_types::worktree_isolation::{MergeResult, RepoLock, WorktreeManager};

// ─── Types ──────────────────────────────────────────────────────────────────

// ExecutionPhase, ProgressCallback, OrchestratorConfig, TeamExecutionStatus
// are re-exported from astra_server_types above.

/// Outcome of a full team execution lifecycle.
#[derive(Debug)]
pub struct TeamExecutionReport {
    pub team_name: String,
    pub delegation_id: String,
    pub parent_run_id: String,
    pub delegation_result: Option<DelegationResult>,
    pub merge_result: Option<MergeResult>,
    /// Branches retained after cancellation so partial isolated work remains
    /// recoverable instead of being deleted by normal cleanup.
    pub preserved_worktree_branches: Vec<String>,
    pub status: TeamExecutionStatus,
    pub error_kind: Option<TeamExecutionErrorKind>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamExecutionErrorKind {
    TeamNotFound,
    InvalidTeam,
    Persistence,
    Execution,
}

impl TeamExecutionErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TeamNotFound => "team_not_found",
            Self::InvalidTeam => "team_invalid",
            Self::Persistence => "team_persistence_error",
            Self::Execution => "team_execution_failed",
        }
    }
}

fn durable_status_for_team_outcome(status: &TeamExecutionStatus) -> &'static str {
    match status {
        TeamExecutionStatus::Completed | TeamExecutionStatus::CompletedWithConflicts => {
            STATUS_COMPLETED
        }
        TeamExecutionStatus::Unfinished
        | TeamExecutionStatus::Partial
        | TeamExecutionStatus::CompletedOverBudget
        | TeamExecutionStatus::Failed => STATUS_FAILED,
    }
}

fn build_execution_profile_snapshot(
    shared_builtins: &AgentProfileRegistry,
    source_agent_id: &str,
    profiles: &[AgentProfile],
) -> Result<AgentProfileRegistry, String> {
    let source = shared_builtins
        .get(source_agent_id)
        .cloned()
        .ok_or_else(|| format!("trusted source agent '{source_agent_id}' is not registered"))?;
    let mut snapshot = AgentProfileRegistry::new();
    snapshot.register(source)?;
    for profile in profiles {
        if shared_builtins.get(&profile.agent_id).is_some() {
            return Err(format!(
                "team member agent_id '{}' collides with a built-in profile",
                profile.agent_id
            ));
        }
        snapshot.register(profile.clone())?;
    }
    Ok(snapshot)
}

// ─── Orchestrator ───────────────────────────────────────────────────────────

/// Orchestrates a full team execution lifecycle.
///
/// Designed to be instantiated per-execution (not long-lived). All state is
/// passed in via constructor args.
pub struct TeamExecutionOrchestrator {
    team_store: Arc<dyn TeamPersistenceService>,
    delegation_engine: Arc<dyn DelegationExecutor>,
    delegation_tracker: Arc<dyn DelegationTracking>,
    run_engine: Arc<dyn RunPersistence>,
    profile_registry: Arc<RwLock<AgentProfileRegistry>>,
    config: OrchestratorConfig,
    repo_lock: RepoLock,
    /// Optional conflict resolver for LLM-assisted merge conflict resolution.
    conflict_resolver: Option<Arc<dyn conflict_resolver::ConflictResolver>>,
    /// Whether this orchestrator is serving the authenticated HTTP Team
    /// boundary. HTTP Team requests do not currently expose optional-tool
    /// selection, so omission must mean an explicit deny. Local CLI Team
    /// execution leaves this disabled to preserve its unmanaged tool policy.
    server_request_boundary: bool,
    /// Optional caller-owned cancellation signal. Server callers normally use
    /// the orchestrator's private token; the local CLI supplies its Ctrl-C
    /// token so cancellation reaches both the child executor and this
    /// lifecycle owner.
    cancellation_token: Option<Arc<tokio_util::sync::CancellationToken>>,
}

impl TeamExecutionOrchestrator {
    pub fn new(
        team_store: Arc<dyn TeamPersistenceService>,
        delegation_engine: Arc<dyn DelegationExecutor>,
        delegation_tracker: Arc<dyn DelegationTracking>,
        run_engine: Arc<dyn RunPersistence>,
        profile_registry: Arc<RwLock<AgentProfileRegistry>>,
        config: OrchestratorConfig,
    ) -> Self {
        Self {
            team_store,
            delegation_engine,
            delegation_tracker,
            run_engine,
            profile_registry,
            config,
            repo_lock: astra_server_types::worktree_isolation::new_repo_lock(),
            conflict_resolver: None,
            server_request_boundary: false,
            cancellation_token: None,
        }
    }

    /// Mark this orchestrator as the authenticated server Team boundary.
    ///
    /// The HTTP request contract has no optional-tool selection field. Make
    /// that absence explicit before the request enters the shared delegation
    /// engine, so an omitted field cannot accidentally inherit the engine's
    /// unmanaged/local default.
    pub fn with_server_request_boundary(mut self) -> Self {
        self.server_request_boundary = true;
        self
    }

    /// Use a caller-owned cancellation signal for the execution lifecycle.
    /// Cancellation remains cooperative and is settled by the orchestrator
    /// before it returns, so callers do not leave durable children behind by
    /// dropping the delegation future.
    pub fn with_cancellation_token(
        mut self,
        token: Arc<tokio_util::sync::CancellationToken>,
    ) -> Self {
        self.cancellation_token = Some(token);
        self
    }

    /// Set a shared repository lock for concurrent team executions.
    pub fn with_repo_lock(mut self, lock: RepoLock) -> Self {
        self.repo_lock = lock;
        self
    }

    /// Enable LLM-assisted merge conflict resolution.
    pub fn with_conflict_resolver(
        mut self,
        resolver: Arc<dyn conflict_resolver::ConflictResolver>,
    ) -> Self {
        self.conflict_resolver = Some(resolver);
        self
    }

    async fn persist_active_run_outcome(&self, run_id: &str, status: &str, error: Option<&str>) {
        match self
            .run_engine
            .persist_status_if_current(astra_services::runs::RunStatusCasRequest {
                user_id: &self.config.user_id,
                expected_session_id: &self.config.session_id,
                run_id,
                expected_statuses: &[STATUS_RUNNING],
                status,
                waiting_for: None,
                error_message: error,
            })
            .await
        {
            Ok(true) => {}
            Ok(false) => tracing::info!(
                run_id,
                attempted_status = status,
                "team outcome did not overwrite a newer durable run decision"
            ),
            Err(persist_error) => tracing::warn!(
                run_id,
                attempted_status = status,
                error = %persist_error,
                "failed to persist team run outcome"
            ),
        }
    }

    /// Execute the full 4-phase lifecycle for a team task.
    pub async fn execute_team(
        &self,
        team_name: &str,
        task: &str,
        repo_root: Option<std::path::PathBuf>,
    ) -> TeamExecutionReport {
        self.execute_team_inner(team_name, task, repo_root, None)
            .await
    }

    /// Execute a direct CLI Team command with its authenticated, frozen model
    /// requirements. The typed sideband is not placed in DelegationRequest.
    pub async fn execute_team_with_model_plan(
        &self,
        team_name: &str,
        task: &str,
        repo_root: Option<std::path::PathBuf>,
        model_plan: astra_turn_types::DirectDelegationModelPlan,
        command_identity: astra_turn_types::DirectDelegationCommandIdentity,
    ) -> TeamExecutionReport {
        self.execute_team_inner(
            team_name,
            task,
            repo_root,
            Some((model_plan, command_identity)),
        )
        .await
    }

    async fn execute_team_inner(
        &self,
        team_name: &str,
        task: &str,
        repo_root: Option<std::path::PathBuf>,
        direct_command: Option<(
            astra_turn_types::DirectDelegationModelPlan,
            astra_turn_types::DirectDelegationCommandIdentity,
        )>,
    ) -> TeamExecutionReport {
        // ── Phase 1: Prepare ────────────────────────────────────────────
        let team = match self
            .team_store
            .load_team(&self.config.user_id, team_name)
            .await
        {
            Ok(Some(t)) => t,
            Ok(None) => {
                return self.fail_report(
                    team_name,
                    "",
                    "",
                    TeamExecutionErrorKind::TeamNotFound,
                    format!("team '{team_name}' not found"),
                );
            }
            Err(e) => {
                return self.fail_report(
                    team_name,
                    "",
                    "",
                    TeamExecutionErrorKind::Persistence,
                    format!("failed to load team: {e}"),
                );
            }
        };

        // A direct command identity is also the durable parent-run identity.
        // The existing run primary key then provides the single admission
        // boundary for retries and concurrent duplicate submissions without a
        // new lookup/table on the normal path. Conversational Team execution
        // keeps its generated run identity.
        let parent_run_id = direct_command
            .as_ref()
            .map(|(_, identity)| identity.command_intent_id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let (request, profiles) =
            match resolve_team(&team, task, &parent_run_id, &self.config.session_id) {
                Ok(resolved) => resolved,
                Err(error) => {
                    return self.fail_report(
                        team_name,
                        "",
                        &parent_run_id,
                        TeamExecutionErrorKind::InvalidTeam,
                        format!("team validation failed: {error}"),
                    );
                }
            };
        let profile_snapshot = {
            let builtins = self.profile_registry.read().await;
            match build_execution_profile_snapshot(
                &builtins,
                &self.config.source_agent_id,
                &profiles,
            ) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return self.fail_report(
                        team_name,
                        "",
                        &parent_run_id,
                        TeamExecutionErrorKind::InvalidTeam,
                        error,
                    );
                }
            }
        };
        if let Some((plan, command_identity)) = &direct_command {
            if request.parent_run_id != command_identity.command_intent_id {
                return self.fail_report(
                    team_name,
                    &request.delegation_id,
                    &parent_run_id,
                    TeamExecutionErrorKind::InvalidTeam,
                    "direct Team command identity was not bound to its parent run".into(),
                );
            }
            let slot_plan = match astra_services::delegation_model_requirement::canonical_team_delegation_slot_plan(
                &request,
                &profiles,
            ) {
                Ok(slot_plan) => slot_plan,
                Err(error) => {
                    return self.fail_report(
                        team_name,
                        "",
                        &parent_run_id,
                        TeamExecutionErrorKind::InvalidTeam,
                        format!("canonical Team model slots are invalid: {error}"),
                    );
                }
            };
            let task_digest = format!("sha256:{:x}", sha2::Sha256::digest(request.task.as_bytes()));
            if let Err(error) = plan.validate_identity(
                command_identity,
                &self.config.user_id,
                &self.config.session_id,
                &task_digest,
                &slot_plan.digest,
                slot_plan.briefs.len(),
            ) {
                return self.fail_report(
                    team_name,
                    "",
                    &parent_run_id,
                    TeamExecutionErrorKind::InvalidTeam,
                    format!("direct Team model plan was rejected: {error}"),
                );
            }
        }
        let delegation_id = request.delegation_id.clone();

        // Start parent durable run only after the complete private execution
        // profile snapshot has been validated.
        if let Err(e) = self
            .run_engine
            .start_run_ext(
                &parent_run_id,
                &self.config.user_id,
                &self.config.session_id,
                None,
                None,
                Some(&self.config.source_agent_id),
                None,
            )
            .await
        {
            return self.fail_report(
                team_name,
                "",
                &parent_run_id,
                TeamExecutionErrorKind::Persistence,
                format!("failed to start run: {e}"),
            );
        }

        // Emit preparation event
        warn_persist!(
            self.run_engine
                .append_event(
                    &self.config.user_id,
                    &self.config.session_id,
                    &parent_run_id,
                    serde_json::json!({
                        "event_type": "team_prepare",
                        "team_name": team_name,
                        "coordination": format!("{:?}", team.coordination),
                        "member_count": team.members.len(),
                        "worktree_mode": format!("{:?}", team.worktree_mode),
                    }),
                )
                .await,
            "Failed to run_engine.append_event"
        );

        // Record execution start (Phase 1 complete, entering execution)
        warn_persist!(
            self.team_store
                .record_execution_start(&delegation_id, &team.team_id, &self.config.user_id, task)
                .await,
            "Failed to team_store.record_execution_start"
        );

        self.emit_progress(ExecutionPhase::Preparing {
            team_name: team_name.to_string(),
            member_count: profiles.len(),
        });

        // Create worktrees if isolated mode
        let mut worktree_mgr = repo_root.map(|root| {
            let mut mgr = WorktreeManager::new(root).with_repo_lock(self.repo_lock.clone());
            if let Some(ref resolver) = self.conflict_resolver {
                mgr = mgr.with_conflict_resolver(resolver.clone(), task.to_string());
            }
            mgr
        });
        let mut preserved_worktree_branches = Vec::new();

        let agent_ids: Vec<String> = profiles.iter().map(|p| p.agent_id.clone()).collect();

        let mut effective_request = request;

        if self.server_request_boundary {
            effective_request.context.insert(
                crate::turn::agentic::delegate_interception::REQUEST_ENABLED_TOOLS_CONTEXT_KEY
                    .to_string(),
                serde_json::json!([]),
            );
        }

        // Inject budget and max_parallel into request context for downstream consumers.
        // `max_duration_secs` is enforced via tokio::time::timeout + CancellationToken.
        // `max_tokens` is enforced post-execution (see token budget check below).
        // `max_cost_usd` is not enforced — no per-model pricing data available yet.
        if let Some(ref budget) = team.budget {
            if let Ok(budget_json) = serde_json::to_value(budget) {
                effective_request
                    .context
                    .insert("team_budget".to_string(), budget_json);
            }
        }
        if team.max_parallel > 0 {
            effective_request.context.insert(
                "team_max_parallel".to_string(),
                serde_json::Value::Number(team.max_parallel.into()),
            );
        }
        if team.worktree_mode == WorktreeMode::Isolated {
            if let Some(ref mut mgr) = worktree_mgr {
                match mgr.create_worktrees(&delegation_id, &agent_ids).await {
                    Ok(paths) => {
                        for (agent_id, path) in &paths {
                            effective_request.context.insert(
                                format!("worktree_path_{agent_id}"),
                                serde_json::Value::String(path.to_string_lossy().to_string()),
                            );
                        }
                        self.emit_progress(ExecutionPhase::WorktreesCreated {
                            agent_ids: agent_ids.clone(),
                        });
                    }
                    Err(e) => {
                        let error = e.to_string();
                        self.persist_active_run_outcome(
                            &parent_run_id,
                            STATUS_FAILED,
                            Some(&error),
                        )
                        .await;
                        return self.fail_report(
                            team_name,
                            &delegation_id,
                            &parent_run_id,
                            TeamExecutionErrorKind::Execution,
                            format!("failed to create worktrees: {e}"),
                        );
                    }
                }
            }
        }

        // Persist checkpoint after preparation phase
        let checkpoint = serde_json::json!({
            "phase": "prepared",
            "delegation_id": &delegation_id,
            "agent_ids": &agent_ids,
            "worktree_mode": format!("{:?}", team.worktree_mode),
        })
        .to_string();
        warn_persist!(
            self.run_engine
                .persist_checkpoint(
                    &self.config.user_id,
                    &self.config.session_id,
                    &parent_run_id,
                    &checkpoint,
                )
                .await,
            "Failed to run_engine.persist_checkpoint"
        );

        // ── Phase 2: Execute ────────────────────────────────────────────
        self.emit_progress(ExecutionPhase::Executing {
            delegation_id: delegation_id.clone(),
        });

        warn_persist!(
            self.run_engine
                .append_event(
                    &self.config.user_id,
                    &self.config.session_id,
                    &parent_run_id,
                    serde_json::json!({
                        "event_type": "team_execute_start",
                        "delegation_id": &delegation_id,
                    }),
                )
                .await,
            "Failed to run_engine.append_event"
        );

        let budget_timeout = team
            .budget
            .as_ref()
            .filter(|b| b.max_duration_secs > 0)
            .map(|b| std::time::Duration::from_secs(b.max_duration_secs));

        // Create a cancellation token for cooperative shutdown of spawned sub-runs.
        // On budget timeout, we cancel this token so fan-out/fork tasks stop promptly
        // instead of being orphaned when the delegation future is dropped.
        let cancel_token = self
            .cancellation_token
            .clone()
            .unwrap_or_else(|| Arc::new(tokio_util::sync::CancellationToken::new()));

        let (model_plan, command_identity) = direct_command
            .map(|(plan, identity)| (Some(plan), Some(identity)))
            .unwrap_or((None, None));
        let delegation_future = self.delegation_engine.execute_delegation(
            effective_request,
            &self.config.source_agent_id,
            profile_snapshot,
            model_plan,
            command_identity,
            Some(cancel_token.clone()),
        );

        // Poll progress during execution (every 500ms) so UI can show real-time updates.
        // This replaces the blocking await with a polling loop that emits AgentProgress.
        let poll_interval = std::time::Duration::from_millis(500);
        let mut last_progress_snapshot: Option<(
            std::collections::HashMap<String, String>,
            usize,
            usize,
        )> = None;

        let delegation_future = Box::pin(delegation_future);
        tokio::pin!(delegation_future);

        // Create budget timeout once (if configured) so it tracks cumulative time
        let budget_deadline = budget_timeout.map(|dur| tokio::time::Instant::now() + dur);

        let mut budget_exceeded_reason = None;
        let delegation_outcome: Result<DelegationResult, String> = loop {
            // Check if budget has been exceeded
            if let Some(deadline) = budget_deadline {
                if tokio::time::Instant::now() >= deadline {
                    budget_exceeded_reason = Some(format!(
                        "team execution exceeded budget timeout of {}s",
                        budget_timeout.map(|d| d.as_secs()).unwrap_or(0)
                    ));
                    cancel_token.cancel();
                    break delegation_future.await;
                }
            }

            tokio::select! {
                biased; // Prefer completion over polling

                result = &mut delegation_future => {
                    break result;
                }
                _ = cancel_token.cancelled() => {
                    // The child executor receives the same token. The
                    // delegation engine owns its bounded cancellation drain
                    // and durable reconciliation; keep awaiting that owner
                    // instead of dropping it at the first Ctrl-C.
                    break delegation_future.await;
                }
                _ = tokio::time::sleep(poll_interval) => {
                    // Poll and emit intermediate progress
                    if let Some(progress) = self
                        .delegation_engine
                        .get_delegation_progress(&delegation_id)
                        .await
                    {
                        let agent_states: std::collections::HashMap<String, String> = progress
                            .agent_states
                            .iter()
                            .map(|(k, v)| (k.clone(), v.as_str().to_string()))
                            .collect();
                        let changed = last_progress_snapshot
                            .as_ref()
                            .map(|(last_states, last_completed, last_total)| {
                                last_states != &agent_states
                                    || *last_completed != progress.completed_count
                                    || *last_total != progress.total_count
                            })
                            .unwrap_or(true);
                        if changed {
                            last_progress_snapshot = Some((
                                agent_states.clone(),
                                progress.completed_count,
                                progress.total_count,
                            ));
                            self.emit_progress(ExecutionPhase::AgentProgress {
                                delegation_id: delegation_id.clone(),
                                agent_states,
                                completed_count: progress.completed_count,
                                total_count: progress.total_count,
                            });
                        }
                    }
                }
            }
        };

        let delegation_result = match delegation_outcome {
            Ok(r) => r,
            Err(e) => {
                let error = budget_exceeded_reason
                    .as_deref()
                    .map(|reason| format!("{reason}; delegation failed: {e}"))
                    .unwrap_or_else(|| e.clone());
                if let Some(ref mut mgr) = worktree_mgr {
                    if cancel_token.is_cancelled() {
                        preserved_worktree_branches = mgr
                            .preserve()
                            .into_iter()
                            .map(|info| info.branch_name)
                            .collect();
                    } else if let Err(ce) = mgr.cleanup().await {
                        eprintln!(
                            "[team-orchestrator] worktree cleanup failed after delegation error: {ce}"
                        );
                    }
                }
                self.persist_preserved_worktree_branches(
                    &parent_run_id,
                    &delegation_id,
                    &preserved_worktree_branches,
                )
                .await;
                self.persist_active_run_outcome(&parent_run_id, STATUS_FAILED, Some(&error))
                    .await;
                // Cleanup delegation state even on error path
                let failure = match self
                    .delegation_tracker
                    .cleanup_delegation(&delegation_id)
                    .await
                {
                    Ok(()) => error.clone(),
                    Err(cleanup_err) => {
                        eprintln!(
                            "[team-orchestrator] delegation cleanup skipped after error: {cleanup_err}"
                        );
                        format!("{error}; cleanup skipped: {cleanup_err}")
                    }
                };
                let mut report = self.fail_report(
                    team_name,
                    &delegation_id,
                    &parent_run_id,
                    TeamExecutionErrorKind::Execution,
                    failure,
                );
                report.preserved_worktree_branches = preserved_worktree_branches;
                return report;
            }
        };

        // Emit final agent progress snapshot
        if let Some(progress) = self
            .delegation_engine
            .get_delegation_progress(&delegation_id)
            .await
        {
            let agent_states: std::collections::HashMap<String, String> = progress
                .agent_states
                .into_iter()
                .map(|(k, v)| (k, v.as_str().to_string()))
                .collect();
            self.emit_progress(ExecutionPhase::AgentProgress {
                delegation_id: delegation_id.clone(),
                agent_states,
                completed_count: progress.completed_count,
                total_count: progress.total_count,
            });
        }

        // Persist token usage from delegation results
        let (total_prompt, total_completion, total_tools) = sum_usage(&delegation_result);
        warn_persist!(
            self.run_engine
                .persist_usage(
                    &self.config.user_id,
                    &self.config.session_id,
                    &parent_run_id,
                    total_prompt,
                    total_completion,
                    total_tools,
                )
                .await,
            "Failed to run_engine.persist_usage"
        );

        // Check token budget (post-execution — tokens are only known after completion)
        let total_tokens = total_prompt + total_completion;
        let exceeded_budget = team
            .budget
            .as_ref()
            .filter(|b| b.max_tokens > 0 && total_tokens > b.max_tokens);
        if let Some(b) = exceeded_budget {
            warn_persist!(
                self.run_engine
                    .append_event(
                        &self.config.user_id,
                        &self.config.session_id,
                        &parent_run_id,
                        serde_json::json!({
                            "event_type": "team_budget_exceeded",
                            "budget_max_tokens": b.max_tokens,
                            "actual_tokens": total_tokens,
                            "enforcement": "post_execution",
                        }),
                    )
                    .await,
                "Failed to run_engine.append_event"
            );
        }

        warn_persist!(
            self.run_engine
                .append_event(
                    &self.config.user_id,
                    &self.config.session_id,
                    &parent_run_id,
                    serde_json::json!({
                        "event_type": "team_execute_complete",
                        "agent_results": delegation_result.agent_results.len(),
                        "total_prompt_tokens": total_prompt,
                        "total_completion_tokens": total_completion,
                    }),
                )
                .await,
            "Failed to run_engine.append_event"
        );

        // ── Phase 3: Merge ──────────────────────────────────────────────
        self.emit_progress(ExecutionPhase::Merging {
            agent_count: delegation_result.agent_results.len(),
        });

        // Shared workspaces have no integration step. Keep that fact explicit:
        // `None` is otherwise ambiguous with cancellation before an isolated
        // merge started.
        let integration_required =
            team.worktree_mode == WorktreeMode::Isolated && worktree_mgr.is_some();
        let merge_result = if !integration_required {
            None
        } else if cancel_token.is_cancelled() {
            // A cancelled child may have produced a partial commit. Never
            // merge an interrupted team into the caller's branch; the durable
            // child outcomes remain available for inspection/recovery.
            None
        } else if team.worktree_mode == WorktreeMode::Isolated {
            if let Some(ref mgr) = worktree_mgr {
                match mgr
                    .merge_worktrees_with_cancellation(
                        &delegation_id,
                        &agent_ids,
                        cancel_token.as_ref(),
                    )
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("[team-orchestrator] worktree merge warning: {e}");
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        // ── Phase 4: Report ─────────────────────────────────────────────
        let conflict_count = merge_result
            .as_ref()
            .map(|m| m.conflicts.len())
            .unwrap_or(0);
        let has_conflicts = conflict_count > 0;

        let (status, error) = derive_team_status(&delegation_result, conflict_count);
        let (status, error) = if let Some(b) = exceeded_budget {
            let msg = format!(
                "token budget exceeded: {total_tokens}/{} tokens",
                b.max_tokens
            );
            let error = Some(match error {
                Some(e) => format!("{e}; {msg}"),
                None => msg,
            });
            // Upgrade status: budget exceeded is a partial failure even if agents succeeded
            let status = match status {
                TeamExecutionStatus::Completed => TeamExecutionStatus::CompletedOverBudget,
                other => other,
            };
            (status, error)
        } else {
            (status, error)
        };
        let (status, error) = if let Some(reason) = budget_exceeded_reason {
            let error = Some(match error {
                Some(error) => format!("{error}; {reason}"),
                None => reason,
            });
            let status = match status {
                TeamExecutionStatus::Completed => TeamExecutionStatus::CompletedOverBudget,
                other => other,
            };
            (status, error)
        } else {
            (status, error)
        };

        // A cancellation that arrives after child execution but before (or
        // during) integration must not be presented as a completed team run.
        // Child results remain useful, but the caller branch was deliberately
        // not updated and the recovery references are the next action. If all
        // merge units already settled before cancellation, preserve the actual
        // completed outcome instead of rewriting history.
        let integration_incomplete = cancel_token.is_cancelled()
            && integration_required
            && match &merge_result {
                None => true,
                Some(result) => {
                    result.merged.len() + result.skipped.len() + result.conflicts.len()
                        < agent_ids.len()
                }
            };
        let (status, error) = if integration_incomplete {
            let integration_message = match &merge_result {
                None => {
                    "execution cancelled before isolated worktree integration began; isolated worktree merge skipped"
                }
                Some(_) => {
                    "execution cancelled during isolated worktree integration; some worktree changes were not merged"
                }
            };
            let error = Some(match error {
                Some(error) => format!("{error}; {integration_message}"),
                None => integration_message.to_string(),
            });
            let status = match status {
                TeamExecutionStatus::Completed
                | TeamExecutionStatus::CompletedWithConflicts
                | TeamExecutionStatus::CompletedOverBudget => TeamExecutionStatus::Unfinished,
                other => other,
            };
            (status, error)
        } else {
            (status, error)
        };

        if cancel_token.is_cancelled()
            && let Some(ref mut mgr) = worktree_mgr
        {
            preserved_worktree_branches = mgr
                .preserve()
                .into_iter()
                .map(|info| info.branch_name)
                .collect();
        }

        self.persist_preserved_worktree_branches(
            &parent_run_id,
            &delegation_id,
            &preserved_worktree_branches,
        )
        .await;

        self.emit_progress(ExecutionPhase::Reporting {
            status: status.clone(),
        });

        // Team report status is a product-level outcome, not a durable run
        // lifecycle state. Keep the detailed value in the report/execution
        // record and project only canonical terminal states to the run row.
        let durable_status = durable_status_for_team_outcome(&status);
        let durable_error = error.clone().or_else(|| {
            (durable_status == STATUS_FAILED)
                .then(|| format!("team execution ended with status {status}"))
        });
        self.persist_active_run_outcome(&parent_run_id, durable_status, durable_error.as_deref())
            .await;

        // Record execution completion (started in Phase 1)
        let result_summary = serde_json::json!({
            "agent_count": delegation_result.agent_results.len(),
            "total_prompt_tokens": total_prompt,
            "total_completion_tokens": total_completion,
            "total_tool_calls": total_tools,
            "has_conflicts": has_conflicts,
            "preserved_worktree_branches": &preserved_worktree_branches,
        });
        warn_persist!(
            self.team_store
                .record_execution_complete(
                    &delegation_id,
                    &status.to_string(),
                    Some(&result_summary.to_string()),
                )
                .await,
            "Failed to team_store.record_execution_complete"
        );

        // Final event
        warn_persist!(
            self.run_engine
                .append_event(
                    &self.config.user_id,
                    &self.config.session_id,
                    &parent_run_id,
                    serde_json::json!({
                        "event_type": "team_complete",
                        "status": status.to_string(),
                        "has_conflicts": has_conflicts,
                        "preserved_worktree_branches": &preserved_worktree_branches,
                    }),
                )
                .await,
            "Failed to run_engine.append_event"
        );

        // Cleanup worktrees after normal completion. Cancellation detached the
        // manager above, keeping its branch/worktree reference recoverable.
        if let Some(ref mut mgr) = worktree_mgr {
            if let Err(ce) = mgr.cleanup().await {
                eprintln!("[team-orchestrator] worktree cleanup failed: {ce}");
            }
        }

        // Cleanup delegation pause flags and progress entries
        if let Err(cleanup_err) = self
            .delegation_tracker
            .cleanup_delegation(&delegation_id)
            .await
        {
            eprintln!("[team-orchestrator] delegation cleanup skipped: {cleanup_err}");
        }

        TeamExecutionReport {
            team_name: team_name.to_string(),
            delegation_id,
            parent_run_id,
            delegation_result: Some(delegation_result),
            merge_result,
            preserved_worktree_branches,
            status,
            error_kind: error.as_ref().map(|_| TeamExecutionErrorKind::Execution),
            error,
        }
    }

    /// Pause all agents in an active team delegation.
    pub async fn pause_team(&self, delegation_id: &str) -> usize {
        self.delegation_tracker
            .pause_delegation(delegation_id)
            .await
    }
    /// Check if a delegation is currently paused.
    pub async fn is_paused(&self, delegation_id: &str) -> bool {
        let sub_runs = self.delegation_tracker.get_sub_runs(delegation_id).await;
        if sub_runs.is_empty() {
            return false;
        }
        // Paused if any sub-run is paused
        for sr in &sub_runs {
            if self.delegation_tracker.is_run_paused(&sr.run_id).await {
                return true;
            }
        }
        false
    }

    fn emit_progress(&self, phase: ExecutionPhase) {
        if let Some(ref cb) = self.config.progress {
            cb(phase);
        }
    }

    /// Keep recovery references in the same durable run timeline as the team
    /// outcome. Normal cleanup has no extra write; cancellation writes one
    /// event only when isolated work remains recoverable.
    async fn persist_preserved_worktree_branches(
        &self,
        parent_run_id: &str,
        delegation_id: &str,
        branches: &[String],
    ) {
        if branches.is_empty() {
            return;
        }
        warn_persist!(
            self.run_engine
                .append_event(
                    &self.config.user_id,
                    &self.config.session_id,
                    parent_run_id,
                    serde_json::json!({
                        "event_type": "team_worktrees_preserved",
                        "delegation_id": delegation_id,
                        "branches": branches,
                        "reason": "cancellation_before_integration",
                    }),
                )
                .await,
            "Failed to persist preserved worktree branches"
        );
    }

    fn fail_report(
        &self,
        team_name: &str,
        delegation_id: &str,
        parent_run_id: &str,
        error_kind: TeamExecutionErrorKind,
        error: String,
    ) -> TeamExecutionReport {
        TeamExecutionReport {
            team_name: team_name.to_string(),
            delegation_id: delegation_id.to_string(),
            parent_run_id: parent_run_id.to_string(),
            delegation_result: None,
            merge_result: None,
            preserved_worktree_branches: Vec::new(),
            status: TeamExecutionStatus::Failed,
            error_kind: Some(error_kind),
            error: Some(error),
        }
    }
}

// Helper functions (sum_usage, summarize_unsuccessful_agents, derive_team_status, etc.)
// are re-exported from astra_server_types::team_orchestrator_types via `pub use` above.

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::delegation::engine::{
        DelegationEngine, DelegationTracker, StubSubRunExecutor, SubRunConfig, SubRunExecutor,
    };
    use crate::server::run::engine::RunEngine;
    use astra_services::coordination::{AgentResult, AgentTier};
    use astra_services::runs::InMemoryRunStateStore;
    use astra_services::team_persistence::{
        InMemoryTeamStore, TeamCoordination, TeamDefinition, TeamMemberDef, WorktreeMode,
    };
    use async_trait::async_trait;
    use std::process::Command;
    use tokio::sync::Notify;

    struct StatusExecutor {
        status: &'static str,
        error: Option<&'static str>,
    }

    #[async_trait]
    impl SubRunExecutor for StatusExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id,
                run_id: config.run_id,
                status: self.status.to_string(),
                output: Some(format!("[{}] yielded", self.status)),
                error: self.error.map(ToString::to_string),
                prompt_tokens: 1,
                completion_tokens: 0,
                tool_calls: 0,
            })
        }
    }

    struct CancellationAwareExecutor {
        started: Arc<Notify>,
    }

    #[async_trait]
    impl SubRunExecutor for CancellationAwareExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            self.started.notify_waiters();
            let token = config
                .cancel_token
                .ok_or_else(|| "cancellation token missing".to_string())?;
            token.cancelled().await;
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id,
                run_id: config.run_id,
                status: astra_core::STATUS_CANCELLED.to_string(),
                output: None,
                error: Some("cancelled by caller".to_string()),
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            })
        }
    }

    struct CommitThenCancelExecutor {
        started: Arc<Notify>,
        wait_for_cancel: bool,
    }

    #[async_trait]
    impl SubRunExecutor for CommitThenCancelExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            let key = format!("worktree_path_{}", config.agent_profile.agent_id);
            let path = config
                .context
                .get(&key)
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("missing isolated worktree context: {key}"))?;
            std::fs::write(
                std::path::Path::new(path).join("cancelled.txt"),
                "must not merge\n",
            )
            .map_err(|error| format!("write test commit: {error}"))?;
            for args in [
                vec!["add", "cancelled.txt"],
                vec!["commit", "-m", "cancelled child"],
            ] {
                let status = Command::new("git")
                    .args(&args)
                    .current_dir(path)
                    .status()
                    .map_err(|error| format!("run git {args:?}: {error}"))?;
                if !status.success() {
                    return Err(format!("git {args:?} failed with {status}"));
                }
            }
            self.started.notify_waiters();
            if !self.wait_for_cancel {
                return Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: STATUS_COMPLETED.to_string(),
                    output: Some("committed child result".to_string()),
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                });
            }
            let token = config
                .cancel_token
                .ok_or_else(|| "cancellation token missing".to_string())?;
            token.cancelled().await;
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id,
                run_id: config.run_id,
                status: astra_core::STATUS_CANCELLED.to_string(),
                output: None,
                error: Some("cancelled by caller".to_string()),
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            })
        }
    }

    async fn setup_orchestrator(team_store: Arc<InMemoryTeamStore>) -> TeamExecutionOrchestrator {
        let registry = Arc::new(RwLock::new(AgentProfileRegistry::new()));

        // Register an Orchestrator-tier agent as the source for delegation validation
        {
            let mut reg = registry.write().await;
            let orch_profile = astra_services::coordination::AgentProfile::new(
                "orchestrator",
                "orchestrator",
                AgentTier::Orchestrator,
            );
            let _ = reg.register(orch_profile);
        }

        let run_store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(run_store));
        let tracker = Arc::new(DelegationTracker::new());

        let delegation = Arc::new(DelegationEngine::with_executor(
            registry.clone(),
            run_engine.clone(),
            tracker.clone(),
            Arc::new(StubSubRunExecutor),
        ));

        TeamExecutionOrchestrator::new(
            team_store,
            delegation,
            tracker,
            run_engine,
            registry,
            OrchestratorConfig {
                user_id: "test-user".to_string(),
                session_id: "test-session".to_string(),
                source_agent_id: "orchestrator".to_string(),
                progress: None,
            },
        )
    }

    #[tokio::test]
    async fn execute_team_not_found() {
        let store = Arc::new(InMemoryTeamStore::new());
        let orch = setup_orchestrator(store).await;

        let report = orch.execute_team("nonexistent", "do something", None).await;
        assert_eq!(report.status, TeamExecutionStatus::Failed);
        assert_eq!(
            report.error_kind,
            Some(TeamExecutionErrorKind::TeamNotFound)
        );
        assert!(report.error.as_ref().unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn execute_team_pipeline_with_stub() {
        let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
        let orch = setup_orchestrator(store).await;

        let report = orch
            .execute_team("research", "analyze codebase", None)
            .await;
        assert_eq!(report.status, TeamExecutionStatus::Completed);
        assert!(report.delegation_result.is_some());
        let dr = report.delegation_result.unwrap();
        assert_eq!(dr.agent_results.len(), 2); // explorer + synthesizer
    }

    #[tokio::test]
    async fn cancellation_settles_parent_and_children_before_returning() {
        let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
        let registry = Arc::new(RwLock::new(AgentProfileRegistry::new()));
        {
            let mut reg = registry.write().await;
            let _ = reg.register(AgentProfile::new(
                "orchestrator",
                "orchestrator",
                AgentTier::Orchestrator,
            ));
        }

        let run_store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(run_store));
        let tracker = Arc::new(DelegationTracker::new());
        let started = Arc::new(Notify::new());
        let delegation = Arc::new(DelegationEngine::with_executor(
            registry.clone(),
            run_engine.clone(),
            tracker.clone(),
            Arc::new(CancellationAwareExecutor {
                started: started.clone(),
            }),
        ));
        let cancellation = Arc::new(tokio_util::sync::CancellationToken::new());
        let orchestrator = TeamExecutionOrchestrator::new(
            store,
            delegation,
            tracker.clone(),
            run_engine.clone(),
            registry,
            OrchestratorConfig {
                user_id: "test-user".to_string(),
                session_id: "test-session".to_string(),
                source_agent_id: "orchestrator".to_string(),
                progress: None,
            },
        )
        .with_cancellation_token(cancellation.clone());

        let execution = tokio::spawn(async move {
            orchestrator
                .execute_team("research", "cancel this task", None)
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
            .await
            .expect("at least one child should start before cancellation");
        cancellation.cancel();
        let report = tokio::time::timeout(std::time::Duration::from_secs(3), execution)
            .await
            .expect("cancellation should settle within its bounded grace period")
            .expect("orchestrator task should not panic");

        assert_ne!(report.status, TeamExecutionStatus::Completed);
        let parent = run_engine
            .load_run("test-user", &report.parent_run_id)
            .await
            .expect("parent run lookup")
            .expect("parent run should remain durable");
        assert_eq!(parent.status, STATUS_FAILED);
        assert!(
            tracker.get_sub_runs(&report.delegation_id).await.is_empty(),
            "cancellation must not leave live tracker children behind"
        );
        for child in report
            .delegation_result
            .expect("cancellation should retain child results")
            .agent_results
        {
            let durable = run_engine
                .load_run("test-user", &child.run_id)
                .await
                .expect("child run lookup")
                .expect("child run should remain durable");
            assert!(
                durable.status == astra_core::STATUS_CANCELLED || durable.status == STATUS_FAILED,
                "child must be terminal after cancellation, got {}",
                durable.status
            );
        }
    }

    #[tokio::test]
    async fn cancellation_does_not_merge_partial_isolated_worktree() {
        let repo = tempfile::tempdir().expect("temporary repository");
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Astra Test"],
        ] {
            assert!(
                Command::new("git")
                    .args(&args)
                    .current_dir(repo.path())
                    .status()
                    .expect("start git")
                    .success(),
                "git {args:?} failed"
            );
        }
        std::fs::write(repo.path().join("README.md"), "base\n").expect("write repository");
        for args in [vec!["add", "README.md"], vec!["commit", "-m", "initial"]] {
            assert!(
                Command::new("git")
                    .args(&args)
                    .current_dir(repo.path())
                    .status()
                    .expect("start git")
                    .success(),
                "git {args:?} failed"
            );
        }

        let store = Arc::new(InMemoryTeamStore::new());
        store
            .save_team(&TeamDefinition {
                team_id: "cancel-isolated-id".to_string(),
                user_id: "test-user".to_string(),
                name: "cancel-isolated".to_string(),
                description: "cancellation merge guard".to_string(),
                coordination: TeamCoordination::Sequential {
                    stop_on_success: false,
                },
                members: vec![TeamMemberDef {
                    role: "worker".to_string(),
                    agent_id: Some("cancel-worker".to_string()),
                    system_prompt: Some("make a change".to_string()),
                    skills: Vec::new(),
                    model_selection: None,
                    mcp_servers: Vec::new(),
                    can_delegate: false,
                    max_delegation_depth: 0,
                }],
                context: std::collections::HashMap::new(),
                worktree_mode: WorktreeMode::Isolated,
                budget: None,
                max_parallel: 0,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .await
            .expect("save isolated cancellation team");

        let registry = Arc::new(RwLock::new(AgentProfileRegistry::new()));
        {
            let mut reg = registry.write().await;
            let _ = reg.register(AgentProfile::new(
                "orchestrator",
                "orchestrator",
                AgentTier::Orchestrator,
            ));
        }
        let run_store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(run_store));
        let tracker = Arc::new(DelegationTracker::new());
        let started = Arc::new(Notify::new());
        let delegation = Arc::new(DelegationEngine::with_executor(
            registry.clone(),
            run_engine.clone(),
            tracker.clone(),
            Arc::new(CommitThenCancelExecutor {
                started: started.clone(),
                wait_for_cancel: false,
            }),
        ));
        let cancellation = Arc::new(tokio_util::sync::CancellationToken::new());
        let progress_cancellation = cancellation.clone();
        let orchestrator = TeamExecutionOrchestrator::new(
            store,
            delegation,
            tracker,
            run_engine.clone(),
            registry,
            OrchestratorConfig {
                user_id: "test-user".to_string(),
                session_id: "test-session".to_string(),
                source_agent_id: "orchestrator".to_string(),
                progress: Some(Arc::new(move |phase| {
                    if matches!(phase, ExecutionPhase::Merging { .. }) {
                        progress_cancellation.cancel();
                    }
                })),
            },
        )
        .with_cancellation_token(cancellation.clone());

        let repo_path = repo.path().to_path_buf();
        let execution = tokio::spawn(async move {
            orchestrator
                .execute_team(
                    "cancel-isolated",
                    "cancel after child commit",
                    Some(repo_path),
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), started.notified())
            .await
            .expect("child should commit before cancellation");
        let report = tokio::time::timeout(std::time::Duration::from_secs(5), execution)
            .await
            .expect("cancellation should settle")
            .expect("orchestrator task should not panic");

        assert_ne!(report.status, TeamExecutionStatus::Completed);
        assert_eq!(report.status, TeamExecutionStatus::Unfinished);
        assert!(
            report
                .error
                .as_deref()
                .is_some_and(|error| error.contains("integration"))
        );
        assert!(
            report.merge_result.is_none(),
            "cancelled teams must not merge partial isolated worktrees"
        );
        assert!(
            !repo.path().join("cancelled.txt").exists(),
            "the cancelled child commit must not reach the caller branch"
        );
        assert_eq!(report.preserved_worktree_branches.len(), 1);
        let branch = &report.preserved_worktree_branches[0];
        let parent = run_engine
            .load_run("test-user", &report.parent_run_id)
            .await
            .expect("parent run lookup")
            .expect("parent run should remain durable");
        assert!(
            parent.events.iter().any(|event| {
                event["event_type"] == "team_worktrees_preserved"
                    && event["branches"]
                        .as_array()
                        .is_some_and(|branches| branches.iter().any(|value| value == branch))
            }),
            "preserved branch must be recorded in the parent run timeline"
        );
        assert!(
            Command::new("git")
                .args(["cat-file", "-e", &format!("{branch}:cancelled.txt")])
                .current_dir(repo.path())
                .status()
                .expect("inspect preserved child branch")
                .success(),
            "cancelled child commit should remain recoverable on {branch}"
        );
    }

    #[tokio::test]
    async fn cancellation_after_shared_children_settled_keeps_completed_outcome() {
        let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
        let registry = Arc::new(RwLock::new(AgentProfileRegistry::new()));
        {
            let mut reg = registry.write().await;
            let _ = reg.register(AgentProfile::new(
                "orchestrator",
                "orchestrator",
                AgentTier::Orchestrator,
            ));
        }
        let run_store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(run_store));
        let tracker = Arc::new(DelegationTracker::new());
        let delegation = Arc::new(DelegationEngine::with_executor(
            registry.clone(),
            run_engine.clone(),
            tracker.clone(),
            Arc::new(StatusExecutor {
                status: STATUS_COMPLETED,
                error: None,
            }),
        ));
        let cancellation = Arc::new(tokio_util::sync::CancellationToken::new());
        let progress_cancellation = cancellation.clone();
        let orchestrator = TeamExecutionOrchestrator::new(
            store,
            delegation,
            tracker,
            run_engine.clone(),
            registry,
            OrchestratorConfig {
                user_id: "test-user".to_string(),
                session_id: "test-session".to_string(),
                source_agent_id: "orchestrator".to_string(),
                progress: Some(Arc::new(move |phase| {
                    if matches!(phase, ExecutionPhase::Merging { .. }) {
                        progress_cancellation.cancel();
                    }
                })),
            },
        )
        .with_cancellation_token(cancellation);

        let report = orchestrator
            .execute_team("research", "finish the shared-workspace task", None)
            .await;

        assert_eq!(report.status, TeamExecutionStatus::Completed);
        assert_eq!(report.error, None);
        assert!(report.merge_result.is_none());
        let parent = run_engine
            .load_run("test-user", &report.parent_run_id)
            .await
            .expect("parent run lookup")
            .expect("parent run should remain durable");
        assert_eq!(parent.status, STATUS_COMPLETED);
    }

    #[tokio::test]
    async fn execute_builtin_review_team_with_stub() {
        let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
        let orch = setup_orchestrator(store).await;

        let report = orch
            .execute_team("review", "review auth module", None)
            .await;
        assert_eq!(report.status, TeamExecutionStatus::Completed);
    }

    #[test]
    fn execution_status_display() {
        assert_eq!(TeamExecutionStatus::Completed.to_string(), "completed");
        assert_eq!(TeamExecutionStatus::Unfinished.to_string(), "unfinished");
        assert_eq!(TeamExecutionStatus::Partial.to_string(), "partial");
        assert_eq!(
            TeamExecutionStatus::CompletedWithConflicts.to_string(),
            "completed_with_conflicts"
        );
        assert_eq!(
            TeamExecutionStatus::CompletedOverBudget.to_string(),
            "completed_over_budget"
        );
        assert_eq!(TeamExecutionStatus::Failed.to_string(), "failed");
        for status in [
            TeamExecutionStatus::Completed,
            TeamExecutionStatus::CompletedWithConflicts,
        ] {
            assert_eq!(durable_status_for_team_outcome(&status), STATUS_COMPLETED);
        }
        for status in [
            TeamExecutionStatus::Unfinished,
            TeamExecutionStatus::Partial,
            TeamExecutionStatus::CompletedOverBudget,
            TeamExecutionStatus::Failed,
        ] {
            assert_eq!(durable_status_for_team_outcome(&status), STATUS_FAILED);
        }
    }

    #[tokio::test]
    async fn execute_team_paused_subrun_reports_unfinished() {
        let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
        let registry = Arc::new(RwLock::new(AgentProfileRegistry::new()));

        {
            let mut reg = registry.write().await;
            let orch = AgentProfile::new("orchestrator", "orchestrator", AgentTier::Orchestrator);
            let _ = reg.register(orch);
        }

        let run_store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(run_store));
        let tracker = Arc::new(DelegationTracker::new());
        let delegation = Arc::new(DelegationEngine::with_executor(
            registry.clone(),
            run_engine.clone(),
            tracker.clone(),
            Arc::new(StatusExecutor {
                status: "paused",
                error: None,
            }),
        ));
        let orch = TeamExecutionOrchestrator::new(
            store,
            delegation,
            tracker,
            run_engine,
            registry,
            OrchestratorConfig {
                user_id: "test-user".to_string(),
                session_id: "test-session".to_string(),
                source_agent_id: "orchestrator".to_string(),
                progress: None,
            },
        );

        let report = orch
            .execute_team("research", "analyze codebase", None)
            .await;
        assert_eq!(report.status, TeamExecutionStatus::Unfinished);
        let dr = report.delegation_result.expect("delegation result");
        assert_eq!(dr.status, "unfinished");
        assert!(
            report
                .error
                .as_deref()
                .is_some_and(|value| value.contains("unfinished"))
        );
    }

    // ─── T-6: Enhanced orchestrator tests ──────────────────────────────

    /// Setup returning orchestrator + run_engine for introspection.
    async fn setup_with_engines(
        team_store: Arc<InMemoryTeamStore>,
    ) -> (
        TeamExecutionOrchestrator,
        Arc<RunEngine>,
        Arc<DelegationTracker>,
    ) {
        let registry = Arc::new(RwLock::new(AgentProfileRegistry::new()));

        {
            let mut reg = registry.write().await;
            let orch = AgentProfile::new("orchestrator", "orchestrator", AgentTier::Orchestrator);
            let _ = reg.register(orch);
        }

        let run_store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(run_store));
        let tracker = Arc::new(DelegationTracker::new());

        let delegation = Arc::new(DelegationEngine::with_executor(
            registry.clone(),
            run_engine.clone(),
            tracker.clone(),
            Arc::new(StubSubRunExecutor),
        ));

        let orch = TeamExecutionOrchestrator::new(
            team_store,
            delegation,
            tracker.clone(),
            run_engine.clone(),
            registry,
            OrchestratorConfig {
                user_id: "test-user".to_string(),
                session_id: "test-session".to_string(),
                source_agent_id: "orchestrator".to_string(),
                progress: None,
            },
        );

        (orch, run_engine, tracker)
    }

    #[tokio::test]
    async fn execute_persists_run_events() {
        let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
        let (orch, run_engine, _) = setup_with_engines(store).await;

        let report = orch.execute_team("research", "analyze", None).await;
        assert_eq!(report.status, TeamExecutionStatus::Completed);

        // The parent run should have events logged
        let run = run_engine
            .load_run("test-user", &report.parent_run_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            run.events.len() >= 3,
            "expected at least 3 events (prepare, exec_start, complete), got {}",
            run.events.len()
        );

        // Verify event types
        let event_types: Vec<String> = run
            .events
            .iter()
            .filter_map(|e| {
                e.get("event_type")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .collect();
        assert!(event_types.contains(&"team_prepare".to_string()));
        assert!(event_types.contains(&"team_execute_start".to_string()));
        assert!(event_types.contains(&"team_complete".to_string()));
    }

    #[tokio::test]
    async fn execute_persists_usage() {
        let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
        let (orch, run_engine, _) = setup_with_engines(store).await;

        let report = orch.execute_team("research", "task", None).await;
        assert_eq!(report.status, TeamExecutionStatus::Completed);

        let run = run_engine
            .load_run("test-user", &report.parent_run_id)
            .await
            .unwrap()
            .unwrap();
        // StubSubRunExecutor produces results with default token counts
        // Usage should have been persisted (even if 0 from stubs)
        assert_eq!(run.status, "completed");
    }

    #[tokio::test]
    async fn direct_team_replay_reuses_run_identity_without_second_execution_tree() {
        use astra_services::delegation_model_requirement::canonical_team_delegation_slot_plan;
        use astra_turn_types::{
            DelegationIntentRequirements, DelegationModelAdmissionOutcome,
            DelegationUserRequirementSource, DirectDelegationCommandIdentity,
        };

        let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
        let (orch, run_engine, _) = setup_with_engines(store.clone()).await;
        let task = "analyze codebase";
        let command = DirectDelegationCommandIdentity {
            command_intent_id: "2bd9f48d-7b44-43d6-92ee-d0f93aa0fbe7".into(),
            session_turn: 1,
        };
        let team = store
            .load_team("test-user", "research")
            .await
            .unwrap()
            .expect("built-in research team");
        let (request, profiles) =
            resolve_team(&team, task, &command.command_intent_id, "test-session").unwrap();
        let slot_plan = canonical_team_delegation_slot_plan(&request, &profiles).unwrap();
        let source = DelegationUserRequirementSource {
            user_id: "test-user".into(),
            session_id: "test-session".into(),
            session_turn: command.session_turn,
            applied_intent_id: None,
            command_intent_id: Some(command.command_intent_id.clone()),
            user_intent_digest: format!("sha256:{:x}", sha2::Sha256::digest(task.as_bytes())),
        };
        let plan = astra_turn_types::DirectDelegationModelPlan {
            source: source.clone(),
            slot_plan_digest: slot_plan.digest,
            outcome: DelegationModelAdmissionOutcome::ExplicitlyUnconstrained {
                slot_count: profiles.len() as u32,
            },
            child_requirements: vec![
                DelegationIntentRequirements::Unconstrained {
                    source: source.clone(),
                };
                profiles.len()
            ],
        };

        let first = orch
            .execute_team_with_model_plan("research", task, None, plan.clone(), command.clone())
            .await;
        assert_eq!(first.status, TeamExecutionStatus::Completed);
        assert_eq!(first.parent_run_id, command.command_intent_id);
        let before = run_engine
            .load_run("test-user", &first.parent_run_id)
            .await
            .unwrap()
            .expect("first command created its durable parent run");

        let second = orch
            .execute_team_with_model_plan("research", task, None, plan, command.clone())
            .await;
        assert_eq!(second.status, TeamExecutionStatus::Failed);
        assert_eq!(second.error_kind, Some(TeamExecutionErrorKind::Persistence));
        assert!(
            second.error.as_deref().is_some_and(
                |error| error.contains("already exists") || error.contains("already bound")
            ),
            "duplicate authenticated command must stop at the run admission boundary: {:?}",
            second.error
        );
        let after = run_engine
            .load_run("test-user", &first.parent_run_id)
            .await
            .unwrap()
            .expect("the first run remains the only durable parent");
        assert_eq!(after.events.len(), before.events.len());
    }

    #[tokio::test]
    async fn execute_persists_checkpoint() {
        let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
        let (orch, run_engine, _) = setup_with_engines(store).await;

        let report = orch.execute_team("research", "task", None).await;
        assert_eq!(report.status, TeamExecutionStatus::Completed);

        let run = run_engine
            .load_run("test-user", &report.parent_run_id)
            .await
            .unwrap()
            .unwrap();
        // Typed checkpoint should be set after preparation phase
        let checkpoint = run_engine
            .load_latest_checkpoint("test-user", &report.parent_run_id, Some("phase"))
            .await
            .unwrap()
            .expect("expected typed checkpoint to be persisted");
        let cp: serde_json::Value = serde_json::from_str(&checkpoint.checkpoint_json).unwrap();
        assert_eq!(cp["phase"], "prepared");
        assert_eq!(run.status, "completed");
    }

    #[test]
    fn concurrent_execution_profile_snapshots_isolate_same_agent_id() {
        let mut builtins = AgentProfileRegistry::new();
        builtins
            .register(AgentProfile::new(
                "orchestrator",
                "orchestrator",
                AgentTier::Orchestrator,
            ))
            .unwrap();
        let mut user_a = AgentProfile::new("shared-name", "worker", AgentTier::User);
        user_a.system_prompt = Some("tenant A prompt".to_string());
        let mut user_b = AgentProfile::new("shared-name", "worker", AgentTier::User);
        user_b.system_prompt = Some("tenant B prompt".to_string());

        let builtins = Arc::new(builtins);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let build = |profile: AgentProfile| {
            let builtins = builtins.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                build_execution_profile_snapshot(&builtins, "orchestrator", &[profile]).unwrap()
            })
        };
        let handle_a = build(user_a);
        let handle_b = build(user_b);
        let snapshot_a = handle_a.join().unwrap();
        let snapshot_b = handle_b.join().unwrap();

        assert_eq!(
            snapshot_a
                .get("shared-name")
                .and_then(|profile| profile.system_prompt.as_deref()),
            Some("tenant A prompt")
        );
        assert_eq!(
            snapshot_b
                .get("shared-name")
                .and_then(|profile| profile.system_prompt.as_deref()),
            Some("tenant B prompt")
        );
        assert!(builtins.get("shared-name").is_none());
    }

    #[test]
    fn execution_profile_snapshot_rejects_builtin_collision_without_pollution() {
        let mut builtins = AgentProfileRegistry::new();
        builtins
            .register(AgentProfile::new(
                "orchestrator",
                "orchestrator",
                AgentTier::Orchestrator,
            ))
            .unwrap();
        let builtin = AgentProfile::new("coder", "builtin coder", AgentTier::System);
        builtins.register(builtin.clone()).unwrap();
        let mut attacker = AgentProfile::new("coder", "tenant override", AgentTier::User);
        attacker.system_prompt = Some("polluted".to_string());

        let error = match build_execution_profile_snapshot(&builtins, "orchestrator", &[attacker]) {
            Ok(_) => panic!("a team must not shadow a built-in profile"),
            Err(error) => error,
        };
        assert!(error.contains("collides with a built-in"));
        let retained = builtins
            .get("coder")
            .expect("builtin must remain registered");
        assert_eq!(retained.name, builtin.name);
        assert_eq!(retained.system_prompt, builtin.system_prompt);
    }

    #[tokio::test]
    async fn progress_callback_receives_phases() {
        let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
        let phases = Arc::new(std::sync::Mutex::new(Vec::new()));
        let phases_clone = phases.clone();

        let registry = Arc::new(RwLock::new(AgentProfileRegistry::new()));
        {
            let mut reg = registry.write().await;
            let orch = AgentProfile::new("orchestrator", "orchestrator", AgentTier::Orchestrator);
            let _ = reg.register(orch);
        }
        let run_store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(run_store));
        let tracker = Arc::new(DelegationTracker::new());
        let delegation = Arc::new(DelegationEngine::with_executor(
            registry.clone(),
            run_engine.clone(),
            tracker.clone(),
            Arc::new(StubSubRunExecutor),
        ));

        let orch = TeamExecutionOrchestrator::new(
            store,
            delegation,
            tracker,
            run_engine,
            registry,
            OrchestratorConfig {
                user_id: "test-user".to_string(),
                session_id: "test-session".to_string(),
                source_agent_id: "orchestrator".to_string(),
                progress: Some(Arc::new(move |phase| {
                    phases_clone.lock().unwrap().push(format!("{phase:?}"));
                })),
            },
        );

        let report = orch.execute_team("research", "task", None).await;
        assert_eq!(report.status, TeamExecutionStatus::Completed);

        let collected = phases.lock().unwrap();
        assert!(
            collected.len() >= 3,
            "expected at least 3 progress phases, got {}",
            collected.len()
        );
        assert!(collected[0].contains("Preparing"));
        assert!(collected.iter().any(|p| p.contains("Executing")));
        assert!(collected.iter().any(|p| p.contains("Reporting")));
    }

    struct SlowSubRunExecutor;

    #[async_trait]
    impl SubRunExecutor for SlowSubRunExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id.clone(),
                run_id: config.run_id.clone(),
                status: "completed".to_string(),
                output: Some("done".to_string()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn progress_callback_emits_intermediate_agent_states() {
        // Virtual time collapses `SlowSubRunExecutor`'s 700ms sleep per agent
        // to instant, dropping the test from ~1.4s real wait to a few ms.
        let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
        let phases = Arc::new(std::sync::Mutex::new(Vec::<ExecutionPhase>::new()));
        let phases_clone = phases.clone();

        let registry = Arc::new(RwLock::new(AgentProfileRegistry::new()));
        {
            let mut reg = registry.write().await;
            let orch = AgentProfile::new("orchestrator", "orchestrator", AgentTier::Orchestrator);
            let _ = reg.register(orch);
        }
        let run_store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(run_store));
        let tracker = Arc::new(DelegationTracker::new());
        let delegation = Arc::new(DelegationEngine::with_executor(
            registry.clone(),
            run_engine.clone(),
            tracker.clone(),
            Arc::new(SlowSubRunExecutor),
        ));

        let orch = TeamExecutionOrchestrator::new(
            store,
            delegation,
            tracker,
            run_engine,
            registry,
            OrchestratorConfig {
                user_id: "test-user".to_string(),
                session_id: "test-session".to_string(),
                source_agent_id: "orchestrator".to_string(),
                progress: Some(Arc::new(move |phase| {
                    phases_clone.lock().unwrap().push(phase);
                })),
            },
        );

        let report = orch.execute_team("research", "task", None).await;
        assert_eq!(report.status, TeamExecutionStatus::Completed);

        let collected = phases.lock().unwrap();
        assert!(collected.iter().any(|phase| {
            matches!(
                phase,
                ExecutionPhase::AgentProgress {
                    agent_states,
                    completed_count: 0,
                    ..
                } if agent_states.values().any(|state| state == "running")
            )
        }));
    }

    #[tokio::test]
    async fn execute_team_validation_failure() {
        let store = Arc::new(InMemoryTeamStore::new());
        // Save a team with empty members (invalid)
        let invalid_team = astra_services::team_persistence::TeamDefinition {
            team_id: "bad-team".to_string(),
            user_id: "test-user".to_string(),
            name: "bad".to_string(),
            description: "Invalid team".to_string(),
            coordination: astra_services::team_persistence::TeamCoordination::Sequential {
                stop_on_success: false,
            },
            members: vec![],
            context: std::collections::HashMap::new(),
            worktree_mode: WorktreeMode::Shared,
            budget: None,
            max_parallel: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let _ = store.save_team(&invalid_team).await;
        let (orch, _, _) = setup_with_engines(store).await;

        let report = orch.execute_team("bad", "task", None).await;
        assert_eq!(report.status, TeamExecutionStatus::Failed);
        assert_eq!(report.error_kind, Some(TeamExecutionErrorKind::InvalidTeam));
        assert!(report.error.as_ref().unwrap().contains("validation failed"));
    }

    #[tokio::test]
    async fn sum_usage_aggregates_correctly() {
        let result = DelegationResult {
            delegation_id: "d1".to_string(),
            status: "completed".to_string(),
            agent_results: vec![
                AgentResult {
                    agent_id: "a1".to_string(),
                    run_id: "r1".to_string(),
                    status: "completed".to_string(),
                    output: None,
                    error: None,
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    tool_calls: 3,
                },
                AgentResult {
                    agent_id: "a2".to_string(),
                    run_id: "r2".to_string(),
                    status: "completed".to_string(),
                    output: None,
                    error: None,
                    prompt_tokens: 200,
                    completion_tokens: 80,
                    tool_calls: 5,
                },
            ],
            aggregated_output: None,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tool_calls: 0,
        };
        let (p, c, t) = sum_usage(&result);
        assert_eq!(p, 300);
        assert_eq!(c, 130);
        assert_eq!(t, 8);
    }

    #[test]
    fn fail_report_helper() {
        let store = Arc::new(InMemoryTeamStore::new());
        let registry = Arc::new(RwLock::new(AgentProfileRegistry::new()));
        let run_store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(run_store));
        let tracker = Arc::new(DelegationTracker::new());

        // We need a sync test, so we construct minimally
        let orch = TeamExecutionOrchestrator {
            team_store: store,
            delegation_engine: Arc::new(DelegationEngine::with_executor(
                registry.clone(),
                run_engine.clone(),
                tracker.clone(),
                Arc::new(StubSubRunExecutor),
            )),
            delegation_tracker: tracker,
            run_engine,
            profile_registry: registry,
            config: OrchestratorConfig {
                user_id: "u".to_string(),
                session_id: "s".to_string(),
                source_agent_id: "o".to_string(),
                progress: None,
            },
            repo_lock: astra_server_types::worktree_isolation::new_repo_lock(),
            conflict_resolver: None,
            server_request_boundary: false,
            cancellation_token: None,
        };

        let report = orch.fail_report(
            "team",
            "deleg",
            "run",
            TeamExecutionErrorKind::Execution,
            "boom".to_string(),
        );
        assert_eq!(report.status, TeamExecutionStatus::Failed);
        assert_eq!(report.team_name, "team");
        assert_eq!(report.delegation_id, "deleg");
        assert_eq!(report.parent_run_id, "run");
        assert_eq!(report.error, Some("boom".to_string()));
        assert_eq!(report.error_kind, Some(TeamExecutionErrorKind::Execution));
    }

    #[tokio::test]
    async fn token_budget_exceeded_emits_event() {
        let store = Arc::new(InMemoryTeamStore::new());
        let team = astra_services::team_persistence::TeamDefinition {
            team_id: "t1".into(),
            user_id: "u1".into(),
            name: "budget-test".into(),
            description: "test".into(),
            coordination: astra_services::team_persistence::TeamCoordination::Sequential {
                stop_on_success: false,
            },
            members: vec![astra_services::team_persistence::TeamMemberDef {
                role: "worker".into(),
                agent_id: None,
                system_prompt: Some("do work".into()),
                skills: vec![],
                model_selection: None,
                mcp_servers: vec![],
                can_delegate: false,
                max_delegation_depth: 0,
            }],
            context: std::collections::HashMap::new(),
            worktree_mode: astra_services::team_persistence::WorktreeMode::Shared,
            budget: Some(astra_services::team_persistence::TeamBudget {
                max_cost_usd: 0.0,
                max_tokens: 100,
                max_duration_secs: 0,
            }),
            max_parallel: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        };
        store.save_team(&team).await.unwrap();

        // Use an executor that returns tokens exceeding the budget
        struct HighTokenExecutor;
        #[async_trait::async_trait]
        impl SubRunExecutor for HighTokenExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some("done".into()),
                    error: None,
                    prompt_tokens: 500,
                    completion_tokens: 500,
                    tool_calls: 0,
                })
            }
        }

        let registry = Arc::new(RwLock::new(AgentProfileRegistry::new()));
        registry
            .write()
            .await
            .register(AgentProfile::new("orch", "orch", AgentTier::Orchestrator))
            .expect("register trusted orchestration source");
        let run_store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(run_store));
        let tracker = Arc::new(DelegationTracker::new());

        let orch = TeamExecutionOrchestrator::new(
            store,
            Arc::new(DelegationEngine::with_executor(
                registry.clone(),
                run_engine.clone(),
                tracker.clone(),
                Arc::new(HighTokenExecutor),
            )),
            tracker,
            run_engine.clone(),
            registry,
            OrchestratorConfig {
                user_id: "u1".into(),
                session_id: "s1".into(),
                source_agent_id: "orch".into(),
                progress: None,
            },
        );

        let report = orch.execute_team("budget-test", "do something", None).await;
        // Budget check is post-execution: run completes but status is upgraded
        assert_eq!(report.status, TeamExecutionStatus::CompletedOverBudget);
        assert!(
            report
                .error
                .as_ref()
                .unwrap()
                .contains("token budget exceeded")
        );
        assert!(report.delegation_result.is_some());
        let durable = run_engine
            .load_run("u1", &report.parent_run_id)
            .await
            .unwrap()
            .expect("team parent run");
        assert_eq!(durable.status, STATUS_FAILED);
        assert!(
            durable
                .error_message
                .as_deref()
                .is_some_and(|error| error.contains("token budget exceeded"))
        );
    }

    /// Executor that returns configurable token counts to trigger budget checks.
    struct TokenBudgetExecutor {
        prompt_tokens: u64,
        completion_tokens: u64,
    }

    #[async_trait]
    impl SubRunExecutor for TokenBudgetExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id,
                run_id: config.run_id,
                status: astra_core::STATUS_COMPLETED.to_string(),
                output: Some("done".into()),
                error: None,
                prompt_tokens: self.prompt_tokens,
                completion_tokens: self.completion_tokens,
                tool_calls: 0,
            })
        }
    }

    #[tokio::test]
    async fn budget_exceeded_event_includes_enforcement_field() {
        let store = Arc::new(InMemoryTeamStore::new());
        let team = astra_services::team_persistence::TeamDefinition {
            team_id: "t-enf".into(),
            user_id: "u1".into(),
            name: "enforce-test".into(),
            description: "test".into(),
            coordination: astra_services::team_persistence::TeamCoordination::Sequential {
                stop_on_success: false,
            },
            members: vec![astra_services::team_persistence::TeamMemberDef {
                role: "worker".into(),
                agent_id: None,
                system_prompt: Some("do work".into()),
                skills: vec![],
                model_selection: None,
                mcp_servers: vec![],
                can_delegate: false,
                max_delegation_depth: 0,
            }],
            context: std::collections::HashMap::new(),
            worktree_mode: astra_services::team_persistence::WorktreeMode::Shared,
            budget: Some(astra_services::team_persistence::TeamBudget {
                max_cost_usd: 0.0,
                max_tokens: 100,
                max_duration_secs: 0,
            }),
            max_parallel: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        };
        store.save_team(&team).await.unwrap();

        let registry = Arc::new(RwLock::new(AgentProfileRegistry::new()));
        registry
            .write()
            .await
            .register(AgentProfile::new("orch", "orch", AgentTier::Orchestrator))
            .expect("register trusted orchestration source");
        let run_store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(run_store));
        let tracker = Arc::new(DelegationTracker::new());

        // Executor returns 500 tokens total, exceeding the 100 budget
        let executor = Arc::new(TokenBudgetExecutor {
            prompt_tokens: 300,
            completion_tokens: 200,
        });

        let orch = TeamExecutionOrchestrator::new(
            store,
            Arc::new(DelegationEngine::with_executor(
                registry.clone(),
                run_engine.clone(),
                tracker.clone(),
                executor,
            )),
            tracker,
            run_engine.clone(),
            registry,
            OrchestratorConfig {
                user_id: "u1".into(),
                session_id: "s1".into(),
                source_agent_id: "orch".into(),
                progress: None,
            },
        );

        let report = orch
            .execute_team("enforce-test", "do something", None)
            .await;

        // Run completes (post-execution check), but error mentions budget
        assert!(
            report
                .error
                .as_ref()
                .map_or(false, |e| e.contains("token budget exceeded")),
            "error should mention budget exceeded, got: {:?}",
            report.error
        );

        // Verify the event carries enforcement=post_execution
        let run = run_engine
            .load_run("u1", &report.parent_run_id)
            .await
            .unwrap()
            .expect("run record should exist");
        let budget_event = run
            .events
            .iter()
            .find(|v| v.get("event_type").and_then(|t| t.as_str()) == Some("team_budget_exceeded"));
        assert!(
            budget_event.is_some(),
            "budget_exceeded event should be emitted"
        );
        let ev = budget_event.unwrap();
        assert_eq!(
            ev.get("enforcement").and_then(|v| v.as_str()),
            Some("post_execution"),
        );
        assert_eq!(ev.get("actual_tokens").and_then(|v| v.as_u64()), Some(500));
        assert_eq!(
            ev.get("budget_max_tokens").and_then(|v| v.as_u64()),
            Some(100)
        );
    }

    // ─── MockHost-driven SubRunExecutor (vs StubSubRunExecutor) ───────────────

    /// Runs `run_agentic_loop_with_host` with one scripted text turn; token fields
    /// mirror SSE accumulators instead of `StubSubRunExecutor`'s zeros.
    struct MockHostSingleTurnSubRunExecutor;

    #[async_trait]
    impl SubRunExecutor for MockHostSingleTurnSubRunExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            use crate::turn::agentic_loop::finalization::run_agentic_loop_with_host;
            use crate::turn::agentic_loop::host::AgenticLoopOutcome;
            use crate::turn::agentic_loop::host::tests::{MockHost, make_state, text_result};
            use astra_core::STATUS_COMPLETED;

            let scripted_output =
                format!("mock-host sub-run ok for task_len={}", config.task.len());
            let mut host = MockHost::new(vec![text_result(&scripted_output, 77, 33, Some(6))]);
            let mut state = make_state();
            state.message = config.task.clone();
            state.user_intent = state.message.clone();

            let outcome = run_agentic_loop_with_host(&mut host, &mut state)
                .await
                .map_err(|e| e.message)?;
            match outcome {
                AgenticLoopOutcome::Completed => {}
                other => return Err(format!("expected Completed, got {other:?}")),
            }

            let prompt_tokens = state.provider_input_tokens();
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id.clone(),
                run_id: config.run_id.clone(),
                status: STATUS_COMPLETED.to_string(),
                output: Some(state.final_text),
                error: None,
                prompt_tokens,
                completion_tokens: state.total_completion,
                tool_calls: state.total_tool_calls,
            })
        }
    }

    /// Two host rounds (edge tool round + final text) so usage reflects multi-turn ingest.
    struct MockHostEdgeThenTextSubRunExecutor;

    #[async_trait]
    impl SubRunExecutor for MockHostEdgeThenTextSubRunExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            use crate::turn::agentic_loop::finalization::run_agentic_loop_with_host;
            use crate::turn::agentic_loop::host::AgenticLoopOutcome;
            use crate::turn::agentic_loop::host::tests::{
                MockHost, edge_tool_result, make_edge_tool, make_state, text_result,
            };
            use astra_core::STATUS_COMPLETED;

            let mut host = MockHost::new(vec![
                edge_tool_result(
                    vec![make_edge_tool("bash", "mock cmd output")],
                    40,
                    12,
                    Some(2),
                ),
                text_result("final after edge", 18, 9, Some(5)),
            ])
            .with_valid_tools(&["bash"]);
            let mut state = make_state();
            state.message = config.task.clone();
            state.user_intent = state.message.clone();

            let outcome = run_agentic_loop_with_host(&mut host, &mut state)
                .await
                .map_err(|e| e.message)?;
            match outcome {
                AgenticLoopOutcome::Completed => {}
                other => return Err(format!("expected Completed, got {other:?}")),
            }

            let prompt_tokens = state.provider_input_tokens();
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id.clone(),
                run_id: config.run_id.clone(),
                status: STATUS_COMPLETED.to_string(),
                output: Some(state.final_text),
                error: None,
                prompt_tokens,
                completion_tokens: state.total_completion,
                tool_calls: state.total_tool_calls,
            })
        }
    }

    #[tokio::test]
    async fn mock_host_subrun_single_turn_nonzero_usage_research_team() {
        let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
        let registry = Arc::new(RwLock::new(AgentProfileRegistry::new()));
        {
            let mut reg = registry.write().await;
            let _ = reg.register(AgentProfile::new(
                "orchestrator",
                "orchestrator",
                AgentTier::Orchestrator,
            ));
        }
        let run_store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(run_store));
        let tracker = Arc::new(DelegationTracker::new());
        let delegation = Arc::new(DelegationEngine::with_executor(
            registry.clone(),
            run_engine.clone(),
            tracker.clone(),
            Arc::new(MockHostSingleTurnSubRunExecutor),
        ));

        let orch = TeamExecutionOrchestrator::new(
            store,
            delegation,
            tracker,
            run_engine,
            registry,
            OrchestratorConfig {
                user_id: "test-user".to_string(),
                session_id: "test-session".to_string(),
                source_agent_id: "orchestrator".to_string(),
                progress: None,
            },
        );

        let report = orch
            .execute_team("research", "analyze codebase", None)
            .await;
        assert_eq!(report.status, TeamExecutionStatus::Completed);
        let dr = report.delegation_result.expect("delegation result");
        assert_eq!(dr.agent_results.len(), 2);
        for ar in &dr.agent_results {
            assert_eq!(
                ar.prompt_tokens, 77,
                "mock host should surface prompt usage"
            );
            assert_eq!(ar.completion_tokens, 33);
            assert!(
                ar.output
                    .as_deref()
                    .unwrap_or("")
                    .contains("mock-host sub-run ok"),
                "output={:?}",
                ar.output
            );
        }
    }

    #[tokio::test]
    async fn mock_host_subrun_edge_then_text_multi_round_usage() {
        let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
        let registry = Arc::new(RwLock::new(AgentProfileRegistry::new()));
        {
            let mut reg = registry.write().await;
            let _ = reg.register(AgentProfile::new(
                "orchestrator",
                "orchestrator",
                AgentTier::Orchestrator,
            ));
        }
        let run_store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(run_store));
        let tracker = Arc::new(DelegationTracker::new());
        let delegation = Arc::new(DelegationEngine::with_executor(
            registry.clone(),
            run_engine.clone(),
            tracker.clone(),
            Arc::new(MockHostEdgeThenTextSubRunExecutor),
        ));

        let orch = TeamExecutionOrchestrator::new(
            store,
            delegation,
            tracker,
            run_engine,
            registry,
            OrchestratorConfig {
                user_id: "test-user".to_string(),
                session_id: "test-session".to_string(),
                source_agent_id: "orchestrator".to_string(),
                progress: None,
            },
        );

        let report = orch.execute_team("research", "task", None).await;
        assert_eq!(report.status, TeamExecutionStatus::Completed);
        let dr = report.delegation_result.expect("delegation result");
        let ar = &dr.agent_results[0];
        assert!(
            ar.prompt_tokens >= 58,
            "expected summed prompt tokens across edge + text rounds (40+18), got {}",
            ar.prompt_tokens
        );
    }
}
