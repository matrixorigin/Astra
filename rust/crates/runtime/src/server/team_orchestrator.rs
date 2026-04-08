//! Team execution orchestrator — 4-phase pipeline bridging `/team run` to `DelegationEngine`.
//!
//! # Phases
//!
//! 1. **Prepare** — load team, resolve profiles, create worktrees, start durable run
//! 2. **Execute** — dispatch through DelegationEngine
//! 3. **Merge** — merge worktrees, aggregate learnings
//! 4. **Report** — persist final status, produce execution summary
//!
//! The orchestrator wraps existing infrastructure (DelegationEngine, RunEngine,
//! WorktreeManager) rather than replacing it.

use std::sync::Arc;

use tokio::sync::RwLock;

use astra_services::coordination::{
    AgentProfile, AgentProfileRegistry, AgentResult, AgentTier, DelegationResult,
};
use astra_services::learning_merge::{AgentLearning, MergedLearning, merge_agent_learnings};
use astra_services::team_persistence::{
    TeamPersistenceService, WorktreeMode,
    team_to_delegation_request,
};

use super::delegation_engine::DelegationEngine;
use super::run_engine::RunEngine;
use super::worktree_isolation::{MergeResult, RepoLock, WorktreeManager};

// ─── Types ──────────────────────────────────────────────────────────────────

/// Configuration for creating a TeamExecutionOrchestrator.
pub struct OrchestratorConfig {
    pub user_id: String,
    pub session_id: String,
    /// Source agent ID requesting the team execution (for delegation validation).
    pub source_agent_id: String,
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
    CompletedWithConflicts,
    Failed,
}

impl std::fmt::Display for TeamExecutionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Completed => write!(f, "completed"),
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
        run_engine: Arc<RunEngine>,
        profile_registry: Arc<RwLock<AgentProfileRegistry>>,
        config: OrchestratorConfig,
    ) -> Self {
        Self {
            team_store,
            delegation_engine,
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
                return TeamExecutionReport {
                    team_name: team_name.to_string(),
                    delegation_id: String::new(),
                    parent_run_id: String::new(),
                    delegation_result: None,
                    merge_result: None,
                    merged_learning: None,
                    status: TeamExecutionStatus::Failed,
                    error: Some(format!("team '{team_name}' not found")),
                };
            }
            Err(e) => {
                return TeamExecutionReport {
                    team_name: team_name.to_string(),
                    delegation_id: String::new(),
                    parent_run_id: String::new(),
                    delegation_result: None,
                    merge_result: None,
                    merged_learning: None,
                    status: TeamExecutionStatus::Failed,
                    error: Some(format!("failed to load team: {e}")),
                };
            }
        };

        // Start parent durable run
        let parent_run_id = uuid::Uuid::new_v4().to_string();
        if let Err(e) = self
            .run_engine
            .start_run(&parent_run_id, &self.config.user_id, &self.config.session_id)
            .await
        {
            return TeamExecutionReport {
                team_name: team_name.to_string(),
                delegation_id: String::new(),
                parent_run_id,
                delegation_result: None,
                merge_result: None,
                merged_learning: None,
                status: TeamExecutionStatus::Failed,
                error: Some(format!("failed to start run: {e}")),
            };
        }

        // Resolve members → profiles and register them, along with
        // a virtual orchestrator profile so delegation validation passes.
        let (request, profiles) = team_to_delegation_request(&team, task, &parent_run_id);
        let delegation_id = request.delegation_id.clone();
        {
            let mut reg = self.profile_registry.write().await;
            // Register the orchestrator as the delegation source
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
        if team.worktree_mode == WorktreeMode::Isolated {
            if let Some(ref mut mgr) = worktree_mgr {
                match mgr.create_worktrees(&delegation_id, &agent_ids).await {
                    Ok(paths) => {
                        for (agent_id, path) in &paths {
                            effective_request.context.insert(
                                format!("worktree_path_{agent_id}"),
                                serde_json::Value::String(
                                    path.to_string_lossy().to_string(),
                                ),
                            );
                        }
                    }
                    Err(e) => {
                        let _ = self
                            .run_engine
                            .persist_status(&parent_run_id, "failed", None, Some(&e.to_string()))
                            .await;
                        return TeamExecutionReport {
                            team_name: team_name.to_string(),
                            delegation_id,
                            parent_run_id,
                            delegation_result: None,
                            merge_result: None,
                            merged_learning: None,
                            status: TeamExecutionStatus::Failed,
                            error: Some(format!("failed to create worktrees: {e}")),
                        };
                    }
                }
            }
        }

        // ── Phase 2: Execute ────────────────────────────────────────────
        let delegation_result = match self
            .delegation_engine
            .execute(effective_request, &self.config.source_agent_id)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let _ = self
                    .run_engine
                    .persist_status(&parent_run_id, "failed", None, Some(&e))
                    .await;
                if let Some(ref mut mgr) = worktree_mgr {
                    if let Err(ce) = mgr.cleanup().await {
                        eprintln!("[team-orchestrator] worktree cleanup failed after delegation error: {ce}");
                    }
                }
                return TeamExecutionReport {
                    team_name: team_name.to_string(),
                    delegation_id,
                    parent_run_id,
                    delegation_result: None,
                    merge_result: None,
                    merged_learning: None,
                    status: TeamExecutionStatus::Failed,
                    error: Some(format!("delegation failed: {e}")),
                };
            }
        };

        // ── Phase 3: Merge ──────────────────────────────────────────────
        let merge_result = if team.worktree_mode == WorktreeMode::Isolated {
            if let Some(ref mgr) = worktree_mgr {
                match mgr.merge_worktrees(&delegation_id, &agent_ids).await {
                    Ok(r) => Some(r),
                    Err(e) => {
                        // Non-fatal: report but continue
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
        let has_conflicts = merge_result
            .as_ref()
            .is_some_and(|m| !m.conflicts.is_empty());

        let status = if has_conflicts {
            TeamExecutionStatus::CompletedWithConflicts
        } else {
            TeamExecutionStatus::Completed
        };

        let _ = self
            .run_engine
            .persist_status(&parent_run_id, &status.to_string(), None, None)
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
            error: None,
        }
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

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
    use super::*;
    use astra_services::coordination::AgentTier;
    use astra_services::runs::InMemoryRunStateStore;
    use astra_services::team_persistence::InMemoryTeamStore;
    use super::super::delegation_engine::{DelegationTracker, StubSubRunExecutor};

    async fn setup_orchestrator(team_store: Arc<InMemoryTeamStore>) -> TeamExecutionOrchestrator {
        let registry = Arc::new(RwLock::new(AgentProfileRegistry::new()));

        // Register an Orchestrator-tier agent as the source for delegation validation
        {
            let mut reg = registry.write().await;
            let orch_profile =
                astra_services::coordination::AgentProfile::new("orchestrator", "orchestrator", AgentTier::Orchestrator);
            let _ = reg.register(orch_profile);
        }

        let run_store = Arc::new(InMemoryRunStateStore::new());
        let run_engine = Arc::new(RunEngine::new(run_store));
        let tracker = Arc::new(DelegationTracker::new());

        let delegation = Arc::new(DelegationEngine::with_executor(
            registry.clone(),
            run_engine.clone(),
            tracker,
            Arc::new(StubSubRunExecutor),
        ));

        TeamExecutionOrchestrator::new(
            team_store,
            delegation,
            run_engine,
            registry,
            OrchestratorConfig {
                user_id: "test-user".to_string(),
                session_id: "test-session".to_string(),
                source_agent_id: "orchestrator".to_string(),
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
        assert_eq!(
            TeamExecutionStatus::CompletedWithConflicts.to_string(),
            "completed_with_conflicts"
        );
        assert_eq!(TeamExecutionStatus::Failed.to_string(), "failed");
    }
}
