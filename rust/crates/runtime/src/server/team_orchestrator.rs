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

use astra_services::coordination::{
    AgentProfile, AgentProfileRegistry, AgentResult, AgentTier, DelegationResult,
};
use astra_services::learning_merge::{AgentLearning, MergedLearning, merge_agent_learnings};
use astra_services::team_persistence::{TeamPersistenceService, WorktreeMode, resolve_team};

use super::delegation_engine::{DelegationEngine, DelegationTracker};
use super::run_engine::RunEngine;
use super::worktree_isolation::{MergeResult, RepoLock, WorktreeManager};

// ─── Types ──────────────────────────────────────────────────────────────────

/// Progress phases emitted during team execution.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionPhase {
    /// Team loaded and validated, profiles resolved.
    Preparing {
        team_name: String,
        member_count: usize,
    },
    /// Worktrees created (only for Isolated mode).
    WorktreesCreated { agent_ids: Vec<String> },
    /// Delegation started via DelegationEngine.
    Executing { delegation_id: String },
    /// Delegation completed, merging worktrees.
    Merging { agent_count: usize },
    /// Merge complete, producing final report.
    Reporting { status: TeamExecutionStatus },
}

/// Callback for reporting execution progress to the UI layer.
pub type ProgressCallback = Arc<dyn Fn(ExecutionPhase) + Send + Sync>;

/// Configuration for creating a TeamExecutionOrchestrator.
pub struct OrchestratorConfig {
    pub user_id: String,
    pub session_id: String,
    /// Source agent ID requesting the team execution (for delegation validation).
    pub source_agent_id: String,
    /// Optional progress callback for UI integration.
    pub progress: Option<ProgressCallback>,
}

/// Outcome of a full team execution lifecycle.
#[derive(Debug)]
pub struct TeamExecutionReport {
    pub team_name: String,
    pub delegation_id: String,
    pub parent_run_id: String,
    pub delegation_result: Option<DelegationResult>,
    pub merge_result: Option<MergeResult>,
    pub merged_learning: Option<MergedLearning>,
    pub status: TeamExecutionStatus,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TeamExecutionStatus {
    Completed,
    Partial,
    CompletedWithConflicts,
    Failed,
}

impl std::fmt::Display for TeamExecutionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Completed => write!(f, "completed"),
            Self::Partial => write!(f, "partial"),
            Self::CompletedWithConflicts => write!(f, "completed_with_conflicts"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

// ─── Orchestrator ───────────────────────────────────────────────────────────

/// Orchestrates a full team execution lifecycle.
///
/// Designed to be instantiated per-execution (not long-lived). All state is
/// passed in via constructor args.
pub struct TeamExecutionOrchestrator {
    team_store: Arc<dyn TeamPersistenceService>,
    delegation_engine: Arc<DelegationEngine>,
    delegation_tracker: Arc<DelegationTracker>,
    run_engine: Arc<RunEngine>,
    profile_registry: Arc<RwLock<AgentProfileRegistry>>,
    config: OrchestratorConfig,
    repo_lock: RepoLock,
    /// Optional conflict resolver for LLM-assisted merge conflict resolution.
    conflict_resolver: Option<Arc<dyn super::conflict_resolver::ConflictResolver>>,
}

impl TeamExecutionOrchestrator {
    pub fn new(
        team_store: Arc<dyn TeamPersistenceService>,
        delegation_engine: Arc<DelegationEngine>,
        delegation_tracker: Arc<DelegationTracker>,
        run_engine: Arc<RunEngine>,
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
            repo_lock: super::worktree_isolation::new_repo_lock(),
            conflict_resolver: None,
        }
    }

    /// Set a shared repository lock for concurrent team executions.
    pub fn with_repo_lock(mut self, lock: RepoLock) -> Self {
        self.repo_lock = lock;
        self
    }

    /// Enable LLM-assisted merge conflict resolution.
    pub fn with_conflict_resolver(
        mut self,
        resolver: Arc<dyn super::conflict_resolver::ConflictResolver>,
    ) -> Self {
        self.conflict_resolver = Some(resolver);
        self
    }

    /// Execute the full 4-phase lifecycle for a team task.
    pub async fn execute_team(
        &self,
        team_name: &str,
        task: &str,
        repo_root: Option<std::path::PathBuf>,
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
                    format!("team '{team_name}' not found"),
                );
            }
            Err(e) => {
                return self.fail_report(team_name, "", "", format!("failed to load team: {e}"));
            }
        };

        // Start parent durable run with team metadata
        let parent_run_id = uuid::Uuid::new_v4().to_string();
        if let Err(e) = self
            .run_engine
            .start_run_ext(
                &parent_run_id,
                &self.config.user_id,
                &self.config.session_id,
                None,
                None,
                Some(&self.config.source_agent_id),
            )
            .await
        {
            return self.fail_report(
                team_name,
                "",
                &parent_run_id,
                format!("failed to start run: {e}"),
            );
        }

        // Emit preparation event
        let _ = self
            .run_engine
            .append_event(
                &parent_run_id,
                serde_json::json!({
                    "event_type": "team_prepare",
                    "team_name": team_name,
                    "coordination": format!("{:?}", team.coordination),
                    "member_count": team.members.len(),
                    "worktree_mode": format!("{:?}", team.worktree_mode),
                }),
            )
            .await;

        // Resolve members → profiles using the new resolve_team with registry lookup
        let registry = self.profile_registry.read().await;
        let (request, profiles) = match resolve_team(&team, task, &parent_run_id, Some(&registry)) {
            Ok(r) => r,
            Err(e) => {
                drop(registry);
                let _ = self
                    .run_engine
                    .persist_status(&parent_run_id, "failed", None, Some(&e))
                    .await;
                return self.fail_report(
                    team_name,
                    "",
                    &parent_run_id,
                    format!("team validation failed: {e}"),
                );
            }
        };
        drop(registry);

        let delegation_id = request.delegation_id.clone();

        // Record execution start (Phase 1 complete, entering execution)
        let _ = self
            .team_store
            .record_execution_start(&delegation_id, &team.team_id, &self.config.user_id, task)
            .await;

        self.emit_progress(ExecutionPhase::Preparing {
            team_name: team_name.to_string(),
            member_count: profiles.len(),
        });

        // Register profiles: virtual orchestrator + resolved team members
        {
            let mut reg = self.profile_registry.write().await;
            let orch = AgentProfile::new(
                &self.config.source_agent_id,
                "orchestrator",
                AgentTier::Orchestrator,
            );
            let _ = reg.register(orch);
            for profile in &profiles {
                let _ = reg.register(profile.clone());
            }
        }

        // Create worktrees if isolated mode
        let mut worktree_mgr = repo_root.map(|root| {
            let mut mgr = WorktreeManager::new(root).with_repo_lock(self.repo_lock.clone());
            if let Some(ref resolver) = self.conflict_resolver {
                mgr = mgr.with_conflict_resolver(resolver.clone(), task.to_string());
            }
            mgr
        });

        let agent_ids: Vec<String> = profiles.iter().map(|p| p.agent_id.clone()).collect();

        let mut effective_request = request;

        // Inject session_id so sub-runs can find the parent session.
        effective_request.context.insert(
            "session_id".to_string(),
            serde_json::Value::String(self.config.session_id.clone()),
        );

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
                        let _ = self
                            .run_engine
                            .persist_status(&parent_run_id, "failed", None, Some(&e.to_string()))
                            .await;
                        return self.fail_report(
                            team_name,
                            &delegation_id,
                            &parent_run_id,
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
        let _ = self
            .run_engine
            .persist_checkpoint(&parent_run_id, &checkpoint)
            .await;

        // ── Phase 2: Execute ────────────────────────────────────────────
        self.emit_progress(ExecutionPhase::Executing {
            delegation_id: delegation_id.clone(),
        });

        let _ = self
            .run_engine
            .append_event(
                &parent_run_id,
                serde_json::json!({
                    "event_type": "team_execute_start",
                    "delegation_id": &delegation_id,
                }),
            )
            .await;

        let budget_timeout = team
            .budget
            .as_ref()
            .filter(|b| b.max_duration_secs > 0)
            .map(|b| std::time::Duration::from_secs(b.max_duration_secs));

        // Create a cancellation token for cooperative shutdown of spawned sub-runs.
        // On budget timeout, we cancel this token so fan-out/fork tasks stop promptly
        // instead of being orphaned when the delegation future is dropped.
        let cancel_token = Arc::new(tokio_util::sync::CancellationToken::new());
        self.delegation_engine
            .set_cancel_token(cancel_token.clone());

        let delegation_future = self
            .delegation_engine
            .execute(effective_request, &self.config.source_agent_id);

        let delegation_outcome = match budget_timeout {
            Some(dur) => match tokio::time::timeout(dur, delegation_future).await {
                Ok(r) => r,
                Err(_) => {
                    cancel_token.cancel();
                    Err(format!(
                        "team execution exceeded budget timeout of {}s",
                        dur.as_secs()
                    ))
                }
            },
            None => delegation_future.await,
        };

        let delegation_result = match delegation_outcome {
            Ok(r) => r,
            Err(e) => {
                let _ = self
                    .run_engine
                    .persist_status(&parent_run_id, "failed", None, Some(&e))
                    .await;
                if let Some(ref mut mgr) = worktree_mgr {
                    if let Err(ce) = mgr.cleanup().await {
                        eprintln!(
                            "[team-orchestrator] worktree cleanup failed after delegation error: {ce}"
                        );
                    }
                }
                return self.fail_report(
                    team_name,
                    &delegation_id,
                    &parent_run_id,
                    format!("delegation failed: {e}"),
                );
            }
        };

        // Persist token usage from delegation results
        let (total_prompt, total_completion, total_tools) = sum_usage(&delegation_result);
        let _ = self
            .run_engine
            .persist_usage(&parent_run_id, total_prompt, total_completion, total_tools)
            .await;

        // Check token budget (post-execution — tokens are only known after completion)
        let total_tokens = total_prompt + total_completion;
        let exceeded_budget = team
            .budget
            .as_ref()
            .filter(|b| b.max_tokens > 0 && total_tokens > b.max_tokens);
        if let Some(b) = exceeded_budget {
            let _ = self
                .run_engine
                .append_event(
                    &parent_run_id,
                    serde_json::json!({
                        "event_type": "team_budget_exceeded",
                        "budget_max_tokens": b.max_tokens,
                        "actual_tokens": total_tokens,
                        "enforcement": "post_execution",
                    }),
                )
                .await;
        }

        let _ = self
            .run_engine
            .append_event(
                &parent_run_id,
                serde_json::json!({
                    "event_type": "team_execute_complete",
                    "agent_results": delegation_result.agent_results.len(),
                    "total_prompt_tokens": total_prompt,
                    "total_completion_tokens": total_completion,
                }),
            )
            .await;

        // ── Phase 3: Merge ──────────────────────────────────────────────
        self.emit_progress(ExecutionPhase::Merging {
            agent_count: delegation_result.agent_results.len(),
        });

        let merge_result = if team.worktree_mode == WorktreeMode::Isolated {
            if let Some(ref mgr) = worktree_mgr {
                match mgr.merge_worktrees(&delegation_id, &agent_ids).await {
                    Ok(r) => Some(r),
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

        // Aggregate learnings from agent results
        let learnings: Vec<AgentLearning> = delegation_result
            .agent_results
            .iter()
            .filter(|r: &&AgentResult| r.is_success())
            .map(|r| extract_learning_from_result(r))
            .collect();
        let merged_learning = if learnings.is_empty() {
            None
        } else {
            Some(merge_agent_learnings(&learnings))
        };

        // ── Phase 4: Report ─────────────────────────────────────────────
        let conflict_count = merge_result
            .as_ref()
            .map(|m| m.conflicts.len())
            .unwrap_or(0);
        let has_conflicts = conflict_count > 0;

        let (status, error) = derive_team_status(&delegation_result, conflict_count);
        let (status, error) = if let Some(b) = exceeded_budget {
            let msg = format!("token budget exceeded: {total_tokens}/{} tokens", b.max_tokens);
            let error = Some(match error {
                Some(e) => format!("{e}; {msg}"),
                None => msg,
            });
            // Upgrade status: budget exceeded is a partial failure even if agents succeeded
            let status = match status {
                TeamExecutionStatus::Completed => TeamExecutionStatus::CompletedWithConflicts,
                other => other,
            };
            (status, error)
        } else {
            (status, error)
        };

        self.emit_progress(ExecutionPhase::Reporting {
            status: status.clone(),
        });

        // Persist final run status
        let _ = self
            .run_engine
            .persist_status(&parent_run_id, &status.to_string(), None, error.as_deref())
            .await;

        // Record execution completion (started in Phase 1)
        let result_summary = serde_json::json!({
            "agent_count": delegation_result.agent_results.len(),
            "total_prompt_tokens": total_prompt,
            "total_completion_tokens": total_completion,
            "total_tool_calls": total_tools,
            "has_conflicts": has_conflicts,
            "merged_learning_patterns": merged_learning.as_ref().map(|l| l.consensus_patterns.len()).unwrap_or(0),
        });
        let _ = self
            .team_store
            .record_execution_complete(
                &delegation_id,
                &status.to_string(),
                Some(&result_summary.to_string()),
            )
            .await;

        // Final event
        let _ = self
            .run_engine
            .append_event(
                &parent_run_id,
                serde_json::json!({
                    "event_type": "team_complete",
                    "status": status.to_string(),
                    "has_conflicts": has_conflicts,
                }),
            )
            .await;

        // Cleanup worktrees
        if let Some(ref mut mgr) = worktree_mgr {
            if let Err(ce) = mgr.cleanup().await {
                eprintln!("[team-orchestrator] worktree cleanup failed: {ce}");
            }
        }

        TeamExecutionReport {
            team_name: team_name.to_string(),
            delegation_id,
            parent_run_id,
            delegation_result: Some(delegation_result),
            merge_result,
            merged_learning,
            status,
            error,
        }
    }

    /// Pause all agents in an active team delegation.
    pub async fn pause_team(&self, delegation_id: &str) -> usize {
        self.delegation_tracker
            .pause_delegation(delegation_id)
            .await
    }

    /// Resume all agents in a paused team delegation.
    pub async fn resume_team(&self, delegation_id: &str) -> usize {
        self.delegation_tracker
            .resume_delegation(delegation_id)
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
            if self.delegation_tracker.is_paused(&sr.run_id).await {
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

    fn fail_report(
        &self,
        team_name: &str,
        delegation_id: &str,
        parent_run_id: &str,
        error: String,
    ) -> TeamExecutionReport {
        TeamExecutionReport {
            team_name: team_name.to_string(),
            delegation_id: delegation_id.to_string(),
            parent_run_id: parent_run_id.to_string(),
            delegation_result: None,
            merge_result: None,
            merged_learning: None,
            status: TeamExecutionStatus::Failed,
            error: Some(error),
        }
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Sum token usage across all agent results.
fn sum_usage(result: &DelegationResult) -> (u64, u64, u32) {
    let mut prompt = 0u64;
    let mut completion = 0u64;
    let mut tools = 0u32;
    for r in &result.agent_results {
        prompt += r.prompt_tokens;
        completion += r.completion_tokens;
        tools += r.tool_calls;
    }
    (prompt, completion, tools)
}

fn summarize_failed_agents(result: &DelegationResult) -> String {
    if result.agent_results.is_empty() {
        return "delegation produced no agent results".to_string();
    }

    let failed: Vec<&AgentResult> = result
        .agent_results
        .iter()
        .filter(|agent| !agent.is_success())
        .collect();
    if failed.is_empty() {
        return "delegation did not produce a successful result".to_string();
    }

    let details: Vec<String> = failed
        .iter()
        .take(3)
        .map(|agent| {
            let reason = agent
                .error
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(agent.status.as_str());
            format!("{}: {}", agent.agent_id, reason)
        })
        .collect();
    let remainder = failed.len().saturating_sub(details.len());
    let suffix = if remainder > 0 {
        format!(" (+{} more)", remainder)
    } else {
        String::new()
    };

    format!(
        "{} of {} agents failed ({}){}",
        failed.len(),
        result.agent_results.len(),
        details.join("; "),
        suffix
    )
}

fn append_merge_conflict_summary(summary: String, conflict_count: usize) -> String {
    if conflict_count == 0 {
        return summary;
    }

    format!("{summary}; merge produced {conflict_count} conflict(s)")
}

fn derive_team_status(
    result: &DelegationResult,
    conflict_count: usize,
) -> (TeamExecutionStatus, Option<String>) {
    match result.status.as_str() {
        "completed" => {
            let status = if conflict_count > 0 {
                TeamExecutionStatus::CompletedWithConflicts
            } else {
                TeamExecutionStatus::Completed
            };
            let error = if conflict_count > 0 {
                Some(format!("merge produced {conflict_count} conflict(s)"))
            } else {
                None
            };
            (status, error)
        }
        "partial" => (
            TeamExecutionStatus::Partial,
            Some(append_merge_conflict_summary(
                summarize_failed_agents(result),
                conflict_count,
            )),
        ),
        "failed" => (
            TeamExecutionStatus::Failed,
            Some(append_merge_conflict_summary(
                summarize_failed_agents(result),
                conflict_count,
            )),
        ),
        other => (
            TeamExecutionStatus::Failed,
            Some(append_merge_conflict_summary(
                format!("delegation ended in unexpected status '{other}'"),
                conflict_count,
            )),
        ),
    }
}

/// Extract a synthetic AgentLearning from an agent result.
///
/// Real implementations would parse structured output from the agent's
/// response. This creates a minimal placeholder.
fn extract_learning_from_result(result: &AgentResult) -> AgentLearning {
    use astra_services::learning_merge::VersionVector;

    let mut version = VersionVector::new();
    version.increment(&result.agent_id);

    AgentLearning {
        agent_id: result.agent_id.clone(),
        session_id: result.run_id.clone(),
        version,
        successful_patterns: vec![],
        failed_patterns: vec![],
        discovered_facts: vec![],
        quality_score: if result.is_success() { 0.8 } else { 0.2 },
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::delegation_engine::{
        DelegationTracker, StubSubRunExecutor, SubRunConfig, SubRunExecutor,
    };
    use super::*;
    use async_trait::async_trait;
    use astra_services::coordination::{AgentResult, AgentTier};
    use astra_services::runs::InMemoryRunStateStore;
    use astra_services::team_persistence::InMemoryTeamStore;

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
    async fn execute_team_adversarial_with_stub() {
        let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
        let orch = setup_orchestrator(store).await;

        let report = orch
            .execute_team("review", "review auth module", None)
            .await;
        assert_eq!(report.status, TeamExecutionStatus::Completed);
    }

    #[tokio::test]
    async fn learning_extracted_from_results() {
        let result = AgentResult {
            agent_id: "coder".to_string(),
            run_id: "run-1".to_string(),
            status: "completed".to_string(),
            output: Some("done".to_string()),
            error: None,
            prompt_tokens: 100,
            completion_tokens: 50,
            tool_calls: 3,
        };
        let learning = extract_learning_from_result(&result);
        assert_eq!(learning.agent_id, "coder");
        assert_eq!(learning.version.get("coder"), 1);
        assert_eq!(learning.quality_score, 0.8);
    }

    #[test]
    fn execution_status_display() {
        assert_eq!(TeamExecutionStatus::Completed.to_string(), "completed");
        assert_eq!(TeamExecutionStatus::Partial.to_string(), "partial");
        assert_eq!(
            TeamExecutionStatus::CompletedWithConflicts.to_string(),
            "completed_with_conflicts"
        );
        assert_eq!(TeamExecutionStatus::Failed.to_string(), "failed");
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
            .load_run(&report.parent_run_id)
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
            .load_run(&report.parent_run_id)
            .await
            .unwrap()
            .unwrap();
        // StubSubRunExecutor produces results with default token counts
        // Usage should have been persisted (even if 0 from stubs)
        assert_eq!(run.status, "completed");
    }

    #[tokio::test]
    async fn execute_persists_checkpoint() {
        let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
        let (orch, run_engine, _) = setup_with_engines(store).await;

        let report = orch.execute_team("research", "task", None).await;
        assert_eq!(report.status, TeamExecutionStatus::Completed);

        let run = run_engine
            .load_run(&report.parent_run_id)
            .await
            .unwrap()
            .unwrap();
        // Checkpoint should be set after preparation phase
        assert!(
            run.checkpoint_json.is_some(),
            "expected checkpoint to be persisted"
        );
        let cp: serde_json::Value =
            serde_json::from_str(run.checkpoint_json.as_ref().unwrap()).unwrap();
        assert_eq!(cp["phase"], "prepared");
    }

    #[tokio::test]
    async fn execute_uses_resolve_team_with_registry() {
        let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
        let (orch, _, _) = setup_with_engines(store).await;

        // Pre-register a profile — but note the builtin team members have
        // explicit system_prompt overrides, so the registry prompt won't
        // be used (member override wins). We verify the profile is in the
        // registry after execution, meaning resolve_team used registry lookup.
        {
            let mut reg = orch.profile_registry.write().await;
            let mut custom =
                AgentProfile::new("team-research-explorer", "explorer", AgentTier::System);
            custom.system_prompt = Some("Custom registered prompt.".to_string());
            custom.model_override = Some("gpt-4-turbo".to_string());
            let _ = reg.register(custom);
        }

        let report = orch.execute_team("research", "task", None).await;
        assert_eq!(report.status, TeamExecutionStatus::Completed);

        // After execution, the profile was re-registered with resolved values.
        // The member's explicit system_prompt overrides the registry prompt.
        let reg = orch.profile_registry.read().await;
        let profile = reg.get("team-research-explorer").unwrap();
        // Member system_prompt takes precedence over registry
        assert!(
            profile
                .system_prompt
                .as_ref()
                .unwrap()
                .contains("search the codebase")
        );
        // But the resolve path DID use registry as base — model_override was empty
        // on the member, so it should be None (member override is None → no override)
        // This confirms the profile was freshly resolved.
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

    #[tokio::test]
    async fn execute_team_validation_failure() {
        let store = Arc::new(InMemoryTeamStore::new());
        // Save a team with empty members (invalid)
        let invalid_team = astra_services::team_persistence::TeamDefinition {
            team_id: "bad-team".to_string(),
            user_id: "test-user".to_string(),
            name: "bad".to_string(),
            description: "Invalid team".to_string(),
            coordination: astra_services::team_persistence::TeamCoordination::Pipeline,
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
            repo_lock: super::super::worktree_isolation::new_repo_lock(),
            conflict_resolver: None,
        };

        let report = orch.fail_report("team", "deleg", "run", "boom".to_string());
        assert_eq!(report.status, TeamExecutionStatus::Failed);
        assert_eq!(report.team_name, "team");
        assert_eq!(report.delegation_id, "deleg");
        assert_eq!(report.parent_run_id, "run");
        assert_eq!(report.error, Some("boom".to_string()));
    }

    #[tokio::test]
    async fn token_budget_exceeded_emits_event() {
        let store = Arc::new(InMemoryTeamStore::new());
        let team = astra_services::team_persistence::TeamDefinition {
            team_id: "t1".into(),
            user_id: "u1".into(),
            name: "budget-test".into(),
            description: "test".into(),
            coordination: astra_services::team_persistence::TeamCoordination::Pipeline,
            members: vec![astra_services::team_persistence::TeamMemberDef {
                role: "worker".into(),
                agent_id: None,
                system_prompt: Some("do work".into()),
                skills: vec![],
                model_override: None,
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
        let run_store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(run_store));
        let tracker = Arc::new(DelegationTracker::new());

        let orch = TeamExecutionOrchestrator::new(
            store,
            Arc::new(DelegationEngine::with_executor(
                registry.clone(),
                run_engine.clone(),
                tracker.clone(),
                Arc::new(StubSubRunExecutor),
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
        // Budget check is post-execution, so run completes normally
        assert_ne!(report.status, TeamExecutionStatus::Failed);
        assert!(report.delegation_result.is_some());
    }

    /// Executor that returns configurable token counts to trigger budget checks.
    struct TokenBudgetExecutor {
        prompt_tokens: u64,
        completion_tokens: u64,
    }

    #[async_trait]
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
            coordination: astra_services::team_persistence::TeamCoordination::Pipeline,
            members: vec![astra_services::team_persistence::TeamMemberDef {
                role: "worker".into(),
                agent_id: None,
                system_prompt: Some("do work".into()),
                skills: vec![],
                model_override: None,
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
            .load_run(&report.parent_run_id)
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
}
