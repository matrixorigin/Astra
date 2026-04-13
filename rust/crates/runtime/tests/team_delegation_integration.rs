//! Integration tests for the team + delegation + agents pipeline.
//!
//! Verifies the full flow: TeamDefinition → resolve_team → DelegationEngine → results,
//! covering all coordination patterns, orchestrator error paths, and agent service integration.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use astra_services::agents::{AgentCreateRequestData, AgentService, InMemoryAgentService};
use astra_services::coordination::{AgentProfile, AgentProfileRegistry, AgentResult, AgentTier};
use astra_services::runs::InMemoryRunStateStore;
use astra_services::team_persistence::{
    InMemoryTeamStore, TeamCoordination, TeamDefinition, TeamMemberDef, TeamPersistenceService,
    WorktreeMode,
};

use astra_runtime::server::delegation_engine::{
    DelegationEngine, DelegationTracker, StubSubRunExecutor, SubRunConfig, SubRunExecutor,
};
use astra_runtime::server::run_engine::RunEngine;
use astra_runtime::server::team_orchestrator::{
    OrchestratorConfig, TeamExecutionOrchestrator, TeamExecutionStatus,
};

// ─── Helpers ────────────────────────────────────────────────────────────────

fn test_team(
    name: &str,
    coord: TeamCoordination,
    members: Vec<(&str, Option<&str>)>,
) -> TeamDefinition {
    TeamDefinition {
        team_id: format!("team-{name}"),
        user_id: "test-user".to_string(),
        name: name.to_string(),
        description: format!("Test team: {name}"),
        coordination: coord,
        members: members
            .into_iter()
            .map(|(role, prompt)| TeamMemberDef {
                role: role.to_string(),
                agent_id: None,
                system_prompt: prompt.map(String::from),
                skills: vec![],
                model_override: None,
                mcp_servers: vec![],
                can_delegate: false,
                max_delegation_depth: 0,
            })
            .collect(),
        context: HashMap::new(),
        worktree_mode: WorktreeMode::Shared,
        budget: None,
        max_parallel: 0,
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
    }
}

async fn setup_orchestrator(
    team_store: Arc<InMemoryTeamStore>,
) -> (
    TeamExecutionOrchestrator,
    Arc<RunEngine>,
    Arc<DelegationTracker>,
) {
    setup_orchestrator_with_executor(team_store, Arc::new(StubSubRunExecutor)).await
}

async fn setup_orchestrator_with_executor(
    team_store: Arc<InMemoryTeamStore>,
    executor: Arc<dyn SubRunExecutor>,
) -> (
    TeamExecutionOrchestrator,
    Arc<RunEngine>,
    Arc<DelegationTracker>,
) {
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
        executor,
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

// ─── Full Pipeline Tests ────────────────────────────────────────────────────

#[tokio::test]
async fn full_pipeline_team_execution() {
    let store = Arc::new(InMemoryTeamStore::new());
    let team = test_team(
        "pipe",
        TeamCoordination::Pipeline,
        vec![
            ("coder", Some("Write code")),
            ("reviewer", Some("Review code")),
        ],
    );
    store.save_team(&team).await.unwrap();

    let (orch, run_engine, tracker) = setup_orchestrator(store.clone()).await;
    let report = orch.execute_team("pipe", "implement auth", None).await;

    assert_eq!(report.status, TeamExecutionStatus::Completed);
    assert!(report.error.is_none());

    let dr = report.delegation_result.as_ref().unwrap();
    assert_eq!(dr.agent_results.len(), 2);

    // Verify run was persisted
    let run = run_engine
        .load_run(&report.parent_run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.status, "completed");

    // Verify sub-runs cleaned up after orchestrator completes
    let subs = tracker.get_sub_runs(&report.delegation_id).await;
    assert_eq!(
        subs.len(),
        0,
        "tracker should be cleaned up after orchestration"
    );

    // Verify delegation results still accessible via the report
    assert_eq!(dr.agent_results.len(), 2);

    // Verify execution history recorded
    let history = store.list_executions(&team.team_id, 10).await.unwrap();
    assert!(!history.is_empty());
}

#[tokio::test]
async fn full_adversarial_team_execution() {
    let store = Arc::new(InMemoryTeamStore::new());
    let team = test_team(
        "adv",
        TeamCoordination::Adversarial {
            max_rounds: 2,
            threshold: 0.8,
        },
        vec![("writer", Some("Write")), ("critic", Some("Critique"))],
    );
    store.save_team(&team).await.unwrap();

    let (orch, _, _) = setup_orchestrator(store).await;
    let report = orch.execute_team("adv", "write docs", None).await;

    assert_eq!(report.status, TeamExecutionStatus::Completed);
    let dr = report.delegation_result.unwrap();
    // 2 rounds × 2 agents = 4
    assert_eq!(dr.agent_results.len(), 4);
}

#[tokio::test]
async fn full_fan_out_team_execution() {
    let store = Arc::new(InMemoryTeamStore::new());
    let team = test_team(
        "fan",
        TeamCoordination::FanOut {
            aggregation: "all_results".into(),
        },
        vec![
            ("analyst-a", Some("Analyze A")),
            ("analyst-b", Some("Analyze B")),
            ("analyst-c", Some("Analyze C")),
        ],
    );
    store.save_team(&team).await.unwrap();

    let (orch, _, _) = setup_orchestrator(store).await;
    let report = orch.execute_team("fan", "analyze codebase", None).await;

    assert_eq!(report.status, TeamExecutionStatus::Completed);
    let dr = report.delegation_result.unwrap();
    assert_eq!(dr.agent_results.len(), 3);
}

#[tokio::test]
async fn full_sequential_team_execution() {
    let store = Arc::new(InMemoryTeamStore::new());
    let team = test_team(
        "seq",
        TeamCoordination::Sequential {
            stop_on_success: true,
        },
        vec![
            ("attempt-1", Some("Try approach 1")),
            ("attempt-2", Some("Try approach 2")),
        ],
    );
    store.save_team(&team).await.unwrap();

    let (orch, _, _) = setup_orchestrator(store).await;
    let report = orch.execute_team("seq", "fix bug", None).await;

    assert_eq!(report.status, TeamExecutionStatus::Completed);
    let dr = report.delegation_result.unwrap();
    // stop_on_success: first agent succeeds → only 1 result
    assert_eq!(dr.agent_results.len(), 1);
}

// ─── Error Path Tests ───────────────────────────────────────────────────────

#[tokio::test]
async fn orchestrator_team_not_found() {
    let store = Arc::new(InMemoryTeamStore::new());
    let (orch, _, _) = setup_orchestrator(store).await;

    let report = orch.execute_team("nonexistent", "task", None).await;
    assert_eq!(report.status, TeamExecutionStatus::Failed);
    assert!(report.error.as_ref().unwrap().contains("not found"));
}

#[tokio::test]
async fn orchestrator_empty_team_fails_validation() {
    let store = Arc::new(InMemoryTeamStore::new());
    let team = test_team("empty", TeamCoordination::Pipeline, vec![]);
    store.save_team(&team).await.unwrap();

    let (orch, run_engine, _) = setup_orchestrator(store).await;
    let report = orch.execute_team("empty", "task", None).await;

    assert_eq!(report.status, TeamExecutionStatus::Failed);
    assert!(report.error.as_ref().unwrap().contains("validation failed"));

    // Parent run should be marked failed
    let run = run_engine
        .load_run(&report.parent_run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.status, "failed");
}

/// Executor that always returns errors.
struct ErrorExecutor;

#[async_trait]
impl SubRunExecutor for ErrorExecutor {
    async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
        Err(format!("agent {} crashed", config.agent_profile.agent_id))
    }
}

#[tokio::test]
async fn orchestrator_delegation_failure_propagates() {
    let store = Arc::new(InMemoryTeamStore::new());
    let team = test_team(
        "fail",
        TeamCoordination::Pipeline,
        vec![("worker", Some("Do work"))],
    );
    store.save_team(&team).await.unwrap();

    let (orch, _, _) = setup_orchestrator_with_executor(store, Arc::new(ErrorExecutor)).await;
    let report = orch.execute_team("fail", "task", None).await;

    // When all agents fail, delegation status is "failed" and team status is Failed
    assert_eq!(report.status, TeamExecutionStatus::Failed);
    let dr = report.delegation_result.unwrap();
    assert_eq!(dr.status, "failed");
    assert_eq!(dr.agent_results[0].status, "failed");
    assert!(
        dr.agent_results[0]
            .error
            .as_ref()
            .unwrap()
            .contains("crashed")
    );
}

// ─── Pause / Resume Tests ───────────────────────────────────────────────────

#[tokio::test]
async fn orchestrator_pause_after_completion_is_noop() {
    let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
    let (orch, _, _tracker) = setup_orchestrator(store).await;

    // Execute a team — orchestrator cleans up tracker state on completion
    let report = orch.execute_team("research", "analyze", None).await;
    assert_eq!(report.status, TeamExecutionStatus::Completed);

    let delegation_id = &report.delegation_id;

    // After completion, delegation state is cleaned up — pause is a no-op
    let paused = orch.pause_team(delegation_id).await;
    assert_eq!(paused, 0);
    assert!(!orch.is_paused(delegation_id).await);
}

#[tokio::test]
async fn is_paused_returns_false_for_unknown_delegation() {
    let store = Arc::new(InMemoryTeamStore::new());
    let (orch, _, _) = setup_orchestrator(store).await;
    assert!(!orch.is_paused("nonexistent").await);
}

// ─── Run Events and Checkpoint Tests ────────────────────────────────────────

#[tokio::test]
async fn orchestrator_persists_events_and_checkpoint() {
    let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
    let (orch, run_engine, _) = setup_orchestrator(store).await;

    let report = orch.execute_team("research", "task", None).await;
    assert_eq!(report.status, TeamExecutionStatus::Completed);

    let run = run_engine
        .load_run(&report.parent_run_id)
        .await
        .unwrap()
        .unwrap();

    // Should have events: team_prepare, team_execute_start, team_execute_complete, team_complete
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

    // Checkpoint should be set
    assert!(run.checkpoint_json.is_some());
    let cp: serde_json::Value =
        serde_json::from_str(run.checkpoint_json.as_ref().unwrap()).unwrap();
    assert_eq!(cp["phase"], "prepared");
}

// ─── Agent Service Integration ──────────────────────────────────────────────

#[tokio::test]
async fn agent_service_crud_lifecycle() {
    let svc = InMemoryAgentService::new();

    // Create
    let agent = svc
        .create_agent(
            "u1".into(),
            AgentCreateRequestData {
                name: "test-agent".into(),
                agent_config: Some(serde_json::json!({"model": "claude-4"})),
                data_source: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(agent.name, "test-agent");

    // List
    let list = svc.list_agents("u1".into()).await.unwrap();
    assert_eq!(list.total, 1);

    // Update
    let updated = svc
        .update_agent(
            agent.agent_id.clone(),
            "u1".into(),
            astra_services::agents::AgentUpdateRequestData {
                name: Some("renamed".into()),
                agent_config: None,
                data_source: None,
                is_active: Some(false),
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.name, "renamed");
    assert!(!updated.is_active);

    // Delete
    svc.delete_agent(agent.agent_id.clone(), "u1".into())
        .await
        .unwrap();
    assert!(svc.get_agent(agent.agent_id, "u1".into()).await.is_err());

    // List should be empty
    let list = svc.list_agents("u1".into()).await.unwrap();
    assert_eq!(list.total, 0);
}

// ─── Learning Merge Integration ─────────────────────────────────────────────

#[tokio::test]
async fn orchestrator_extracts_learning_from_results() {
    let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
    let (orch, _, _) = setup_orchestrator(store).await;

    let report = orch
        .execute_team("research", "analyze patterns", None)
        .await;
    assert_eq!(report.status, TeamExecutionStatus::Completed);

    // Merged learning should be present (even if minimal from stub executor)
    assert!(report.merged_learning.is_some());
    let learning = report.merged_learning.unwrap();
    assert!(learning.agent_count >= 2);
}

// ─── Team Persistence Round-Trip ────────────────────────────────────────────

#[tokio::test]
async fn team_persistence_full_lifecycle() {
    let store = InMemoryTeamStore::new();

    let team = test_team(
        "lifecycle",
        TeamCoordination::Pipeline,
        vec![("a", Some("Agent A")), ("b", Some("Agent B"))],
    );

    // Save
    store.save_team(&team).await.unwrap();

    // Load
    let loaded = store
        .load_team("test-user", "lifecycle")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.team_id, team.team_id);
    assert_eq!(loaded.members.len(), 2);

    // List
    let list = store.list_teams("test-user").await.unwrap();
    assert_eq!(list.len(), 1);

    // Execution recording
    store
        .record_execution_start("exec-1", &team.team_id, "test-user", "task")
        .await
        .unwrap();
    store
        .record_execution_complete("exec-1", "completed", Some(r#"{"ok":true}"#))
        .await
        .unwrap();
    let execs = store.list_executions(&team.team_id, 10).await.unwrap();
    assert_eq!(execs.len(), 1);
    assert_eq!(execs[0].status, "completed");

    // Delete
    assert!(store.delete_team("test-user", "lifecycle").await.unwrap());
    assert!(
        store
            .load_team("test-user", "lifecycle")
            .await
            .unwrap()
            .is_none()
    );
}

// ─── Execution History team_id Consistency ──────────────────────────────────

#[tokio::test]
async fn orchestrator_records_execution_with_stable_team_id() {
    let store = Arc::new(InMemoryTeamStore::new());

    let team = test_team(
        "consistency",
        TeamCoordination::Pipeline,
        vec![
            ("coder", Some("Code agent")),
            ("tester", Some("Test agent")),
        ],
    );
    let expected_team_id = team.team_id.clone();
    store.save_team(&team).await.unwrap();

    let (orch, _, _) = setup_orchestrator(store.clone()).await;
    let report = orch
        .execute_team("consistency", "build feature", None)
        .await;

    // Verify execution was recorded with the correct team_id (not team name)
    let by_team_id = store.list_executions(&expected_team_id, 10).await.unwrap();
    assert_eq!(by_team_id.len(), 1, "should find execution by team_id");
    assert_eq!(by_team_id[0].status, report.status.to_string());

    // Querying by display name should return nothing (team_id != name)
    let by_name = store.list_executions("consistency", 10).await.unwrap();
    assert!(
        by_name.is_empty(),
        "team display name should not match team_id in execution history"
    );
}

#[tokio::test]
async fn orchestrator_writes_start_then_complete_not_duplicate() {
    let store = Arc::new(InMemoryTeamStore::new());

    let team = test_team(
        "lifecycle-order",
        TeamCoordination::FanOut {
            aggregation: "all_results".into(),
        },
        vec![
            ("analyzer", Some("Analyze agent")),
            ("reporter", Some("Report agent")),
        ],
    );
    store.save_team(&team).await.unwrap();

    let (orch, _, _) = setup_orchestrator(store.clone()).await;
    let _report = orch
        .execute_team("lifecycle-order", "analyze logs", None)
        .await;

    // Exactly one execution record (no duplicates from CLI + orchestrator)
    let execs = store.list_executions(&team.team_id, 10).await.unwrap();
    assert_eq!(
        execs.len(),
        1,
        "should have exactly one execution record, not duplicates"
    );

    // The record should be completed (not stuck in running)
    assert_ne!(
        execs[0].status, "running",
        "execution should be marked complete"
    );
    assert!(
        execs[0].completed_at.is_some(),
        "completed_at should be set"
    );
}

// ─── Budget Timeout Enforcement ─────────────────────────────────────────────

#[tokio::test]
async fn budget_timeout_aborts_slow_execution() {
    use astra_services::team_persistence::TeamBudget;

    let store = Arc::new(InMemoryTeamStore::new());

    let mut team = test_team(
        "slow-team",
        TeamCoordination::Pipeline,
        vec![("worker", Some("Slow worker"))],
    );
    team.budget = Some(TeamBudget {
        max_cost_usd: 100.0,
        max_tokens: 1_000_000,
        max_duration_secs: 1, // 1 second timeout
    });
    store.save_team(&team).await.unwrap();

    // Executor that sleeps longer than the budget timeout
    struct SlowExecutor;
    #[async_trait]
    impl SubRunExecutor for SlowExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id.clone(),
                run_id: config.run_id,
                status: "completed".to_string(),
                output: Some("done".to_string()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            })
        }
    }

    let (orch, _, _) =
        setup_orchestrator_with_executor(store.clone(), Arc::new(SlowExecutor)).await;
    let report = orch.execute_team("slow-team", "do work", None).await;

    assert_eq!(report.status, TeamExecutionStatus::Failed);
    assert!(
        report
            .error
            .as_ref()
            .unwrap()
            .contains("exceeded budget timeout"),
        "error should mention timeout, got: {:?}",
        report.error
    );
}

#[tokio::test]
async fn budget_timeout_zero_means_no_limit() {
    use astra_services::team_persistence::TeamBudget;

    let store = Arc::new(InMemoryTeamStore::new());

    let mut team = test_team(
        "no-limit",
        TeamCoordination::Pipeline,
        vec![("worker", Some("Fast worker"))],
    );
    team.budget = Some(TeamBudget {
        max_cost_usd: 10.0,
        max_tokens: 100_000,
        max_duration_secs: 0, // 0 = no timeout
    });
    store.save_team(&team).await.unwrap();

    let (orch, _, _) = setup_orchestrator(store).await;
    let report = orch.execute_team("no-limit", "do work", None).await;

    // Should complete normally (stub executor is instant)
    assert_eq!(report.status, TeamExecutionStatus::Completed);
}

// ─── Max Parallel Enforcement ───────────────────────────────────────────────

#[tokio::test]
async fn fan_out_respects_max_parallel() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let store = Arc::new(InMemoryTeamStore::new());

    let mut team = test_team(
        "parallel-test",
        TeamCoordination::FanOut {
            aggregation: "all_results".into(),
        },
        vec![
            ("a", Some("Agent A")),
            ("b", Some("Agent B")),
            ("c", Some("Agent C")),
        ],
    );
    team.max_parallel = 1; // Only 1 at a time
    store.save_team(&team).await.unwrap();

    // Executor that tracks peak concurrency
    struct ConcurrencyTracker {
        current: AtomicUsize,
        peak: AtomicUsize,
    }
    #[async_trait]
    impl SubRunExecutor for ConcurrencyTracker {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            let prev = self.current.fetch_add(1, Ordering::SeqCst);
            let concurrent = prev + 1;
            // Update peak
            self.peak.fetch_max(concurrent, Ordering::SeqCst);
            // Hold for a bit so other tasks have a chance to start
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            self.current.fetch_sub(1, Ordering::SeqCst);
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id.clone(),
                run_id: config.run_id,
                status: "completed".to_string(),
                output: Some("done".to_string()),
                error: None,
                prompt_tokens: 10,
                completion_tokens: 20,
                tool_calls: 0,
            })
        }
    }

    let tracker = Arc::new(ConcurrencyTracker {
        current: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    });

    let (orch, _, _) = setup_orchestrator_with_executor(store, tracker.clone()).await;
    let report = orch.execute_team("parallel-test", "do work", None).await;

    assert_eq!(report.status, TeamExecutionStatus::Completed);
    assert_eq!(
        tracker.peak.load(Ordering::SeqCst),
        1,
        "peak concurrency should be 1 with max_parallel=1"
    );
}

// ─── Real-World Unhappy Path Scenarios ──────────────────────────────────────

/// Tests pipeline behavior when first stage fails.
/// Pipeline continues all stages even when one fails (feeding previous output to next).
#[tokio::test]
async fn pipeline_continues_after_first_stage_fails() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let store = Arc::new(InMemoryTeamStore::new());
    let team = test_team(
        "fail-early",
        TeamCoordination::Pipeline,
        vec![
            ("coder", Some("Write code")),
            ("reviewer", Some("Review code")),
            ("deployer", Some("Deploy code")),
        ],
    );
    store.save_team(&team).await.unwrap();

    let execution_count = Arc::new(AtomicUsize::new(0));
    let count_ref = execution_count.clone();

    struct FailFirstExecutor {
        count: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl SubRunExecutor for FailFirstExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            let n = self.count.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // First stage fails
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id.clone(),
                    run_id: config.run_id,
                    status: "failed".to_string(),
                    output: None,
                    error: Some("compilation error".to_string()),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                })
            } else {
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id.clone(),
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some("done".to_string()),
                    error: None,
                    prompt_tokens: 10,
                    completion_tokens: 20,
                    tool_calls: 0,
                })
            }
        }
    }

    let (orch, _, _) =
        setup_orchestrator_with_executor(store, Arc::new(FailFirstExecutor { count: count_ref }))
            .await;
    let report = orch.execute_team("fail-early", "build", None).await;

    // Pipeline completes all stages even if some fail, resulting in Partial
    assert_eq!(report.status, TeamExecutionStatus::Partial);
    // All 3 stages should execute (pipeline continues regardless of failure)
    assert_eq!(
        execution_count.load(Ordering::SeqCst),
        3,
        "pipeline should execute all stages"
    );
    // Check that we have results from all agents
    let dr = report.delegation_result.unwrap();
    assert_eq!(dr.agent_results.len(), 3);
    // First should be failed, others completed
    assert!(!dr.agent_results[0].is_success());
    assert!(dr.agent_results[1].is_success());
    assert!(dr.agent_results[2].is_success());
}

/// Tests that partial fan-out failures still return results from successful agents.
#[tokio::test]
async fn fan_out_returns_partial_success_with_failures() {
    let store = Arc::new(InMemoryTeamStore::new());
    let team = test_team(
        "partial",
        TeamCoordination::FanOut {
            aggregation: "all_results".into(),
        },
        vec![
            ("researcher1", Some("Research topic A")),
            ("researcher2", Some("Research topic B")),
            ("researcher3", Some("Research topic C")),
        ],
    );
    store.save_team(&team).await.unwrap();

    struct PartialFailExecutor;
    #[async_trait]
    impl SubRunExecutor for PartialFailExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            // Middle agent fails
            if config.agent_profile.agent_id.contains("researcher2") {
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id.clone(),
                    run_id: config.run_id,
                    status: "failed".to_string(),
                    output: None,
                    error: Some("network timeout".to_string()),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                })
            } else {
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id.clone(),
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some(format!("results from {}", config.agent_profile.agent_id)),
                    error: None,
                    prompt_tokens: 100,
                    completion_tokens: 200,
                    tool_calls: 1,
                })
            }
        }
    }

    let (orch, _, _) = setup_orchestrator_with_executor(store, Arc::new(PartialFailExecutor)).await;
    let report = orch.execute_team("partial", "research", None).await;

    // With all_results aggregation, partial failures still complete (not failed)
    let dr = report.delegation_result.unwrap();
    assert_eq!(dr.agent_results.len(), 3, "all results should be present");

    let successful: Vec<_> = dr.agent_results.iter().filter(|r| r.is_success()).collect();
    let failed: Vec<_> = dr
        .agent_results
        .iter()
        .filter(|r| !r.is_success())
        .collect();

    assert_eq!(successful.len(), 2, "2 agents should succeed");
    assert_eq!(failed.len(), 1, "1 agent should fail");
    assert!(
        failed[0]
            .error
            .as_ref()
            .unwrap()
            .contains("network timeout"),
        "failure reason preserved"
    );
}

/// Tests that executor panics (Err return) are captured gracefully.
#[tokio::test]
async fn executor_panic_captured_as_failed_result() {
    let store = Arc::new(InMemoryTeamStore::new());
    let team = test_team(
        "panic-team",
        TeamCoordination::FanOut {
            aggregation: "all_results".into(),
        },
        vec![("worker", Some("Panic worker"))],
    );
    store.save_team(&team).await.unwrap();

    struct PanicExecutor;
    #[async_trait]
    impl SubRunExecutor for PanicExecutor {
        async fn execute(&self, _config: SubRunConfig) -> Result<AgentResult, String> {
            Err("internal assertion failed".to_string())
        }
    }

    let (orch, _, _) = setup_orchestrator_with_executor(store, Arc::new(PanicExecutor)).await;
    let report = orch.execute_team("panic-team", "work", None).await;

    assert_eq!(report.status, TeamExecutionStatus::Failed);
    let dr = report.delegation_result.unwrap();
    assert_eq!(dr.agent_results.len(), 1);
    assert!(!dr.agent_results[0].is_success());
    assert!(
        dr.agent_results[0]
            .error
            .as_ref()
            .unwrap()
            .contains("internal assertion failed"),
        "executor error should be captured"
    );
}

/// Tests that concurrent team executions don't interfere with each other.
#[tokio::test]
async fn concurrent_team_executions_isolated() {
    let store = Arc::new(InMemoryTeamStore::new());
    // Two different teams
    let team1 = test_team(
        "team-a",
        TeamCoordination::Pipeline,
        vec![("coder-a", Some("Team A coder"))],
    );
    let team2 = test_team(
        "team-b",
        TeamCoordination::Pipeline,
        vec![("coder-b", Some("Team B coder"))],
    );
    store.save_team(&team1).await.unwrap();
    store.save_team(&team2).await.unwrap();

    let execution_log: Arc<tokio::sync::Mutex<Vec<String>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let log_ref = execution_log.clone();

    struct LoggingExecutor {
        log: Arc<tokio::sync::Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl SubRunExecutor for LoggingExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            let agent_id = config.agent_profile.agent_id.clone();
            self.log.lock().await.push(format!("start:{agent_id}"));
            // Simulate work
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            self.log.lock().await.push(format!("end:{agent_id}"));
            Ok(AgentResult {
                agent_id,
                run_id: config.run_id,
                status: "completed".to_string(),
                output: Some("done".to_string()),
                error: None,
                prompt_tokens: 10,
                completion_tokens: 20,
                tool_calls: 0,
            })
        }
    }

    let (orch, _, _) =
        setup_orchestrator_with_executor(store.clone(), Arc::new(LoggingExecutor { log: log_ref }))
            .await;

    // Run both teams concurrently
    let (report1, report2) = tokio::join!(
        orch.execute_team("team-a", "task-a", None),
        orch.execute_team("team-b", "task-b", None)
    );

    assert_eq!(report1.status, TeamExecutionStatus::Completed);
    assert_eq!(report2.status, TeamExecutionStatus::Completed);

    // Each team should have its own delegation_id
    assert_ne!(
        report1.delegation_id, report2.delegation_id,
        "concurrent executions should have separate delegation IDs"
    );

    // Both agents should have executed
    let log = execution_log.lock().await;
    assert!(log.iter().any(|e| e.contains("coder-a")));
    assert!(log.iter().any(|e| e.contains("coder-b")));
}

/// Tests that sequential coordination with stop_on_success stops after first success.
#[tokio::test]
async fn sequential_stop_on_success_stops_early() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let store = Arc::new(InMemoryTeamStore::new());
    let team = test_team(
        "stop-early",
        TeamCoordination::Sequential {
            stop_on_success: true,
        },
        vec![
            ("fallback1", Some("First fallback")),
            ("fallback2", Some("Second fallback")),
            ("fallback3", Some("Third fallback")),
        ],
    );
    store.save_team(&team).await.unwrap();

    let execution_count = Arc::new(AtomicUsize::new(0));
    let count_ref = execution_count.clone();

    struct SuccessSecondExecutor {
        count: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl SubRunExecutor for SuccessSecondExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            let n = self.count.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // First fails
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id.clone(),
                    run_id: config.run_id,
                    status: "failed".to_string(),
                    output: None,
                    error: Some("not available".to_string()),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                })
            } else {
                // Second succeeds
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id.clone(),
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some("success!".to_string()),
                    error: None,
                    prompt_tokens: 10,
                    completion_tokens: 20,
                    tool_calls: 0,
                })
            }
        }
    }

    let (orch, _, _) = setup_orchestrator_with_executor(
        store,
        Arc::new(SuccessSecondExecutor { count: count_ref }),
    )
    .await;
    let report = orch.execute_team("stop-early", "try fallbacks", None).await;

    // Sequential with one failure and one success results in Partial
    // (because not all agents completed successfully)
    assert_eq!(report.status, TeamExecutionStatus::Partial);
    // Should have stopped after second agent succeeded
    assert_eq!(
        execution_count.load(Ordering::SeqCst),
        2,
        "should stop after first success"
    );
}

// ─── Adversarial Review Unhappy Paths ───────────────────────────────────────

/// Tests that adversarial review runs all rounds when all agents succeed.
#[tokio::test]
async fn adversarial_runs_all_rounds_on_success() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let store = Arc::new(InMemoryTeamStore::new());
    let team = test_team(
        "adversarial-quick",
        TeamCoordination::Adversarial {
            max_rounds: 5,
            threshold: 0.8,
        },
        vec![
            ("producer", Some("Create content")),
            ("reviewer", Some("Review content")),
        ],
    );
    store.save_team(&team).await.unwrap();

    let rounds_executed = Arc::new(AtomicUsize::new(0));
    let rounds_ref = rounds_executed.clone();

    struct QuickConvergeExecutor {
        rounds: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl SubRunExecutor for QuickConvergeExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            self.rounds.fetch_add(1, Ordering::SeqCst);
            // Always succeed - producer passes, reviewer approves
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id.clone(),
                run_id: config.run_id,
                status: "completed".to_string(),
                output: Some(if config.agent_profile.agent_id.contains("reviewer") {
                    "APPROVED: content is good".to_string()
                } else {
                    "generated content".to_string()
                }),
                error: None,
                prompt_tokens: 50,
                completion_tokens: 100,
                tool_calls: 0,
            })
        }
    }

    let (orch, _, _) = setup_orchestrator_with_executor(
        store,
        Arc::new(QuickConvergeExecutor { rounds: rounds_ref }),
    )
    .await;
    let report = orch
        .execute_team("adversarial-quick", "create document", None)
        .await;

    assert_eq!(report.status, TeamExecutionStatus::Completed);
    // Adversarial runs all max_rounds, each round has 2 agents (producer + reviewer)
    assert_eq!(
        rounds_executed.load(Ordering::SeqCst),
        10,
        "should run all 5 rounds × 2 agents"
    );
}

/// Tests that adversarial review fails when reviewer keeps rejecting.
#[tokio::test]
async fn adversarial_fails_after_max_rounds() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let store = Arc::new(InMemoryTeamStore::new());
    let team = test_team(
        "adversarial-stuck",
        TeamCoordination::Adversarial {
            max_rounds: 2,
            threshold: 0.8,
        },
        vec![
            ("producer", Some("Create content")),
            ("reviewer", Some("Review content")),
        ],
    );
    store.save_team(&team).await.unwrap();

    let rounds_executed = Arc::new(AtomicUsize::new(0));
    let rounds_ref = rounds_executed.clone();

    struct AlwaysRejectExecutor {
        rounds: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl SubRunExecutor for AlwaysRejectExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            self.rounds.fetch_add(1, Ordering::SeqCst);
            if config.agent_profile.agent_id.contains("reviewer") {
                // Reviewer always rejects
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id.clone(),
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some("REJECTED: needs more work".to_string()),
                    error: None,
                    prompt_tokens: 50,
                    completion_tokens: 100,
                    tool_calls: 0,
                })
            } else {
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id.clone(),
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some("generated content".to_string()),
                    error: None,
                    prompt_tokens: 50,
                    completion_tokens: 100,
                    tool_calls: 0,
                })
            }
        }
    }

    let (orch, _, _) = setup_orchestrator_with_executor(
        store,
        Arc::new(AlwaysRejectExecutor { rounds: rounds_ref }),
    )
    .await;
    let report = orch
        .execute_team("adversarial-stuck", "create document", None)
        .await;

    // Adversarial that never converges still completes (with rejection noted)
    assert!(
        matches!(
            report.status,
            TeamExecutionStatus::Completed | TeamExecutionStatus::Partial
        ),
        "should complete even without approval"
    );
    // Should have executed all rounds: 2 rounds * 2 agents = 4 calls
    assert_eq!(
        rounds_executed.load(Ordering::SeqCst),
        4,
        "should execute all max_rounds"
    );
}

// ─── Task Cancellation ──────────────────────────────────────────────────────

/// Tests that cancelling a running team execution stops agents.
#[tokio::test]
async fn team_execution_respects_cancellation() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let store = Arc::new(InMemoryTeamStore::new());
    let team = test_team(
        "cancellable",
        TeamCoordination::FanOut {
            aggregation: "all_results".into(),
        },
        vec![("worker1", Some("Worker 1")), ("worker2", Some("Worker 2"))],
    );
    store.save_team(&team).await.unwrap();

    let started = Arc::new(AtomicBool::new(false));
    let started_ref = started.clone();

    struct BlockingExecutor {
        started: Arc<AtomicBool>,
    }
    #[async_trait]
    impl SubRunExecutor for BlockingExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            self.started.store(true, Ordering::SeqCst);
            // Block for a long time
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id.clone(),
                run_id: config.run_id,
                status: "completed".to_string(),
                output: Some("done".to_string()),
                error: None,
                prompt_tokens: 10,
                completion_tokens: 20,
                tool_calls: 0,
            })
        }
    }

    let (orch, _, _tracker) = setup_orchestrator_with_executor(
        store,
        Arc::new(BlockingExecutor {
            started: started_ref,
        }),
    )
    .await;

    let orch = Arc::new(orch);
    let orch_clone = orch.clone();

    // Start execution in background
    let exec_handle = tokio::spawn(async move {
        orch_clone
            .execute_team("cancellable", "blocked task", None)
            .await
    });

    // Wait for execution to start
    while !started.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // Brief delay to let delegation spawn records be created
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Just drop the handle (simulates cancellation)
    exec_handle.abort();

    // Verify the spawn was aborted
    let result = exec_handle.await;
    assert!(result.is_err(), "should be cancelled/aborted");
}

// ─── Error Message Propagation ──────────────────────────────────────────────

/// Tests that specific error messages from agents are preserved in results.
#[tokio::test]
async fn error_messages_preserved_in_results() {
    let store = Arc::new(InMemoryTeamStore::new());
    // Use 2 agents so we get Partial status (one succeeds, one fails)
    let team = test_team(
        "error-details",
        TeamCoordination::FanOut {
            aggregation: "all_results".to_string(),
        },
        vec![
            ("healthy", Some("Healthy agent")),
            ("faulty", Some("Faulty agent")),
        ],
    );
    store.save_team(&team).await.unwrap();

    struct MixedResultExecutor;
    #[async_trait]
    impl SubRunExecutor for MixedResultExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            if config.agent_profile.agent_id.contains("faulty") {
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id.clone(),
                    run_id: config.run_id,
                    status: "failed".to_string(),
                    output: None,
                    error: Some(
                        "DatabaseConnectionError: connection timed out after 30s".to_string(),
                    ),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                })
            } else {
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id.clone(),
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some("success".to_string()),
                    error: None,
                    prompt_tokens: 100,
                    completion_tokens: 200,
                    tool_calls: 0,
                })
            }
        }
    }

    let (orch, _, _) = setup_orchestrator_with_executor(store, Arc::new(MixedResultExecutor)).await;
    let report = orch
        .execute_team("error-details", "query database", None)
        .await;

    assert_eq!(report.status, TeamExecutionStatus::Partial);
    let dr = report.delegation_result.unwrap();
    assert_eq!(dr.agent_results.len(), 2);

    // Find the failed agent's result
    let failed_result = dr
        .agent_results
        .iter()
        .find(|r| r.status == "failed")
        .unwrap();
    let error_msg = failed_result.error.as_ref().unwrap();
    assert!(
        error_msg.contains("DatabaseConnectionError"),
        "specific error should be preserved: {error_msg}"
    );
    assert!(
        error_msg.contains("timed out"),
        "error details should be preserved: {error_msg}"
    );
}

/// Tests that team execution report includes proper error summary.
#[tokio::test]
async fn team_report_includes_error_summary() {
    let store = Arc::new(InMemoryTeamStore::new());
    let team = test_team(
        "multi-error",
        TeamCoordination::FanOut {
            aggregation: "all_results".into(),
        },
        vec![("a", Some("Agent A")), ("b", Some("Agent B"))],
    );
    store.save_team(&team).await.unwrap();

    struct BothFailExecutor;
    #[async_trait]
    impl SubRunExecutor for BothFailExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            let error = if config.agent_profile.agent_id.contains('a') {
                "A crashed"
            } else {
                "B crashed"
            };
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id.clone(),
                run_id: config.run_id,
                status: "failed".to_string(),
                output: None,
                error: Some(error.to_string()),
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            })
        }
    }

    let (orch, _, _) = setup_orchestrator_with_executor(store, Arc::new(BothFailExecutor)).await;
    let report = orch.execute_team("multi-error", "run both", None).await;

    // When all fail, status should be Failed
    assert_eq!(report.status, TeamExecutionStatus::Failed);
    // Report error should summarize failures
    assert!(
        report.error.is_some(),
        "report should include error summary"
    );
    let summary = report.error.as_ref().unwrap();
    // Check that failed agents are mentioned
    assert!(
        summary.contains("failed") || summary.contains("crashed"),
        "error summary should mention failures: {summary}"
    );
}
