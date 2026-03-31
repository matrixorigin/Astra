//! Delegation engine — spawns and tracks sub-runs for multi-agent coordination.
//!
//! Bridges [`CoordinationPattern`] from the services crate with [`RunEngine`]
//! for actual execution. Enforces depth limits, tracks parent→child relationships,
//! and aggregates results.
//!
//! # Example Flow (FanOut)
//!
//! ```text
//! Orchestrator run-A
//!   ├── delegate(FanOut{agent_ids: [s1, s2]})
//!   │     ├── sub-run-B (agent s1)  ──▶ completed ✅
//!   │     └── sub-run-C (agent s2)  ──▶ completed ✅
//!   │
//!   └── aggregate(results) ──▶ merged output
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use mo_agent_services::coordination::{
    AgentProfile, AgentProfileRegistry, AgentResult, AggregationStrategy, CoordinationPattern,
    DelegationRequest, DelegationResult, aggregate_results,
};

use super::run_engine::RunEngine;

// ─── Sub-run Executor Trait ─────────────────────────────────────────────────

/// Configuration for a sub-run spawned by delegation.
#[derive(Debug, Clone)]
pub struct SubRunConfig {
    /// Unique ID for this sub-run.
    pub run_id: String,
    /// Agent profile executing this sub-run.
    pub agent_profile: AgentProfile,
    /// The task/prompt for this sub-run.
    pub task: String,
    /// Parent's session ID (sub-runs share the session lineage).
    pub session_id: String,
    /// User ID owning the delegation.
    pub user_id: String,
    /// Optional output from previous pipeline stage.
    pub previous_output: Option<String>,
    /// Context key-value pairs from the delegation request.
    pub context: HashMap<String, serde_json::Value>,
}

/// Trait for executing sub-runs as part of a delegation.
///
/// Production implementations use [`ServerAgenticLoopHost`] to run a real
/// agentic loop. Test implementations return mock results.
#[async_trait]
pub trait SubRunExecutor: Send + Sync {
    /// Execute a sub-run and return the result.
    async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String>;
}

/// No-op executor that immediately returns "completed" results.
/// Used when no real executor is wired (tests, offline mode).
pub struct StubSubRunExecutor;

#[async_trait]
impl SubRunExecutor for StubSubRunExecutor {
    async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
        Ok(AgentResult {
            agent_id: config.agent_profile.agent_id,
            run_id: config.run_id,
            status: "completed".to_string(),
            output: Some(format!("[stub] completed task: {}", config.task)),
            error: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
        })
    }
}

// ─── Sub-run Tracking ───────────────────────────────────────────────────────

/// Tracks parent→child relationships for delegation hierarchies.
#[derive(Debug, Clone)]
pub struct SubRunRecord {
    /// The sub-run's own ID.
    pub run_id: String,
    /// Parent run that spawned this sub-run.
    pub parent_run_id: String,
    /// Delegation this sub-run belongs to.
    pub delegation_id: String,
    /// Agent executing this sub-run.
    pub agent_id: String,
    /// Current depth in the delegation tree.
    pub depth: u32,
}

/// In-memory tracker for delegation hierarchies.
pub struct DelegationTracker {
    /// delegation_id → sub-run records
    delegations: RwLock<HashMap<String, Vec<SubRunRecord>>>,
    /// run_id → parent_run_id (for quick lookups)
    parents: RwLock<HashMap<String, String>>,
}

impl DelegationTracker {
    pub fn new() -> Self {
        Self {
            delegations: RwLock::new(HashMap::new()),
            parents: RwLock::new(HashMap::new()),
        }
    }

    /// Record a sub-run spawned by a delegation.
    pub async fn record_sub_run(&self, record: SubRunRecord) {
        let run_id = record.run_id.clone();
        let parent_id = record.parent_run_id.clone();
        let delegation_id = record.delegation_id.clone();

        self.delegations
            .write()
            .await
            .entry(delegation_id)
            .or_default()
            .push(record);

        self.parents.write().await.insert(run_id, parent_id);
    }

    /// Get all sub-runs for a delegation.
    pub async fn get_sub_runs(&self, delegation_id: &str) -> Vec<SubRunRecord> {
        self.delegations
            .read()
            .await
            .get(delegation_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Get the parent run ID for a given run.
    pub async fn get_parent(&self, run_id: &str) -> Option<String> {
        self.parents.read().await.get(run_id).cloned()
    }

    /// Check if a run is a sub-run (has a parent).
    pub async fn is_sub_run(&self, run_id: &str) -> bool {
        self.parents.read().await.contains_key(run_id)
    }

    /// Get all sub-run IDs for a given parent run across all delegations.
    pub async fn get_children(&self, parent_run_id: &str) -> Vec<String> {
        self.parents
            .read()
            .await
            .iter()
            .filter(|(_, parent)| parent.as_str() == parent_run_id)
            .map(|(child, _)| child.clone())
            .collect()
    }

    /// Get the agent_id for a run. Returns `None` for top-level (non-sub) runs.
    pub async fn get_agent_id(&self, run_id: &str) -> Option<String> {
        for records in self.delegations.read().await.values() {
            for record in records {
                if record.run_id == run_id {
                    return Some(record.agent_id.clone());
                }
            }
        }
        None
    }

    /// Get the full ancestry chain (run_id → parent → grandparent → ...).
    pub async fn get_ancestry(&self, run_id: &str) -> Vec<String> {
        let parents = self.parents.read().await;
        let mut chain = Vec::new();
        let mut current = run_id.to_string();
        while let Some(parent) = parents.get(&current) {
            chain.push(parent.clone());
            current = parent.clone();
        }
        chain
    }
}

impl Default for DelegationTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Delegation Engine ──────────────────────────────────────────────────────

/// Engine for executing multi-agent delegations.
///
/// Validates delegation requests against the agent profile registry,
/// spawns sub-runs via RunEngine, tracks hierarchies via DelegationTracker,
/// and **executes** them via [`SubRunExecutor`].
pub struct DelegationEngine {
    /// Agent profiles for validation.
    registry: Arc<RwLock<AgentProfileRegistry>>,
    /// Run engine for spawning sub-runs.
    run_engine: Arc<RunEngine>,
    /// Tracks parent→child run relationships.
    tracker: Arc<DelegationTracker>,
    /// Executor for actually running sub-agent loops.
    executor: Arc<dyn SubRunExecutor>,
}

impl DelegationEngine {
    pub fn new(
        registry: Arc<RwLock<AgentProfileRegistry>>,
        run_engine: Arc<RunEngine>,
        tracker: Arc<DelegationTracker>,
    ) -> Self {
        Self {
            registry,
            run_engine,
            tracker,
            executor: Arc::new(StubSubRunExecutor),
        }
    }

    /// Create engine with a real sub-run executor.
    pub fn with_executor(
        registry: Arc<RwLock<AgentProfileRegistry>>,
        run_engine: Arc<RunEngine>,
        tracker: Arc<DelegationTracker>,
        executor: Arc<dyn SubRunExecutor>,
    ) -> Self {
        Self {
            registry,
            run_engine,
            tracker,
            executor,
        }
    }

    /// Validate a delegation request without executing it.
    pub async fn validate(
        &self,
        request: &DelegationRequest,
        source_agent_id: &str,
    ) -> Result<(), String> {
        let reg = self.registry.read().await;
        reg.validate_delegation(request, source_agent_id)
    }

    /// Execute a delegation: spawn sub-runs according to the coordination pattern.
    ///
    /// Returns a `DelegationResult` with individual agent results and
    /// aggregated output. Sub-runs are created in the RunEngine and tracked
    /// in the DelegationTracker for hierarchy queries.
    pub async fn execute(
        &self,
        request: DelegationRequest,
        source_agent_id: &str,
    ) -> Result<DelegationResult, String> {
        // Validate first
        self.validate(&request, source_agent_id).await?;

        match &request.pattern {
            CoordinationPattern::FanOut {
                agent_ids,
                aggregation,
                ..
            } => self.execute_fan_out(&request, agent_ids, aggregation).await,
            CoordinationPattern::Pipeline { stages } => {
                let agent_ids: Vec<String> = stages.iter().map(|s| s.agent_id.clone()).collect();
                self.execute_sequential(&request, &agent_ids, false).await
            }
            CoordinationPattern::Sequential {
                agent_ids,
                stop_on_success,
            } => {
                self.execute_sequential(&request, agent_ids, *stop_on_success)
                    .await
            }
            CoordinationPattern::AdversarialReview {
                producer_id,
                reviewer_id,
                max_rounds,
                ..
            } => {
                self.execute_adversarial(&request, producer_id, reviewer_id, *max_rounds)
                    .await
            }
        }
    }

    /// Fan-out: spawn all agents in parallel, aggregate results.
    async fn execute_fan_out(
        &self,
        request: &DelegationRequest,
        agent_ids: &[String],
        aggregation: &AggregationStrategy,
    ) -> Result<DelegationResult, String> {
        let reg = self.registry.read().await;

        // Build configs + create runs in parallel
        let mut configs = Vec::new();
        for agent_id in agent_ids {
            let sub_run_id = uuid::Uuid::new_v4().to_string();

            self.run_engine
                .start_run(&sub_run_id, &request.user_id, &sub_run_id)
                .await?;

            self.tracker
                .record_sub_run(SubRunRecord {
                    run_id: sub_run_id.clone(),
                    parent_run_id: request.parent_run_id.clone(),
                    delegation_id: request.delegation_id.clone(),
                    agent_id: agent_id.clone(),
                    depth: request.depth + 1,
                })
                .await;

            self.run_engine
                .persist_status(&sub_run_id, "running", Some("agent_execution"), None)
                .await?;

            let profile = reg
                .get(agent_id)
                .cloned()
                .unwrap_or_else(|| AgentProfile::new(agent_id, agent_id, mo_agent_services::coordination::AgentTier::User));

            configs.push(SubRunConfig {
                run_id: sub_run_id,
                agent_profile: profile,
                task: request.task.clone(),
                session_id: request
                    .context
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("delegation")
                    .to_string(),
                user_id: request.user_id.clone(),
                previous_output: None,
                context: request.context.clone(),
            });
        }
        drop(reg);

        // Execute all sub-runs in parallel via tokio tasks.
        let mut handles = Vec::new();
        for config in configs {
            let executor = self.executor.clone();
            let run_engine = self.run_engine.clone();
            handles.push(tokio::spawn(async move {
                let run_id = config.run_id.clone();
                let result = executor.execute(config).await;
                // Persist final status
                match &result {
                    Ok(r) => {
                        let _ = run_engine
                            .persist_status(&run_id, &r.status, None, r.error.as_deref())
                            .await;
                    }
                    Err(e) => {
                        let _ = run_engine
                            .persist_status(&run_id, "failed", None, Some(e.as_str()))
                            .await;
                    }
                }
                result
            }));
        }

        let mut results = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(Ok(result)) => results.push(result),
                Ok(Err(e)) => {
                    results.push(AgentResult {
                        agent_id: "unknown".to_string(),
                        run_id: String::new(),
                        status: "failed".to_string(),
                        output: None,
                        error: Some(e),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    });
                }
                Err(e) => {
                    results.push(AgentResult {
                        agent_id: "unknown".to_string(),
                        run_id: String::new(),
                        status: "failed".to_string(),
                        output: None,
                        error: Some(format!("task join error: {}", e)),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    });
                }
            }
        }

        let aggregated = aggregate_results(aggregation, &results);
        Ok(DelegationResult::from_results(
            &request.delegation_id,
            results,
            aggregated,
        ))
    }

    /// Sequential / Pipeline: execute agents one after another.
    /// Pipeline feeds previous output to the next agent.
    async fn execute_sequential(
        &self,
        request: &DelegationRequest,
        agent_ids: &[String],
        stop_on_success: bool,
    ) -> Result<DelegationResult, String> {
        let reg = self.registry.read().await;
        let mut results = Vec::new();
        let mut previous_output: Option<String> = None;

        for agent_id in agent_ids {
            let sub_run_id = uuid::Uuid::new_v4().to_string();

            self.run_engine
                .start_run(&sub_run_id, &request.user_id, &sub_run_id)
                .await?;

            self.tracker
                .record_sub_run(SubRunRecord {
                    run_id: sub_run_id.clone(),
                    parent_run_id: request.parent_run_id.clone(),
                    delegation_id: request.delegation_id.clone(),
                    agent_id: agent_id.clone(),
                    depth: request.depth + 1,
                })
                .await;

            self.run_engine
                .persist_status(&sub_run_id, "running", Some("agent_execution"), None)
                .await?;

            let profile = reg
                .get(agent_id)
                .cloned()
                .unwrap_or_else(|| AgentProfile::new(agent_id, agent_id, mo_agent_services::coordination::AgentTier::User));

            let config = SubRunConfig {
                run_id: sub_run_id.clone(),
                agent_profile: profile,
                task: request.task.clone(),
                session_id: request
                    .context
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("delegation")
                    .to_string(),
                user_id: request.user_id.clone(),
                previous_output: previous_output.clone(),
                context: request.context.clone(),
            };

            let result = match self.executor.execute(config).await {
                Ok(r) => {
                    let _ = self
                        .run_engine
                        .persist_status(&sub_run_id, &r.status, None, r.error.as_deref())
                        .await;
                    r
                }
                Err(e) => {
                    let _ = self
                        .run_engine
                        .persist_status(&sub_run_id, "failed", None, Some(&e))
                        .await;
                    AgentResult {
                        agent_id: agent_id.clone(),
                        run_id: sub_run_id,
                        status: "failed".to_string(),
                        output: None,
                        error: Some(e),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    }
                }
            };

            // Feed output to the next stage (pipeline semantics).
            previous_output = result.output.clone();
            let is_success = result.is_success();
            results.push(result);

            if stop_on_success && is_success {
                break;
            }
        }

        Ok(DelegationResult::from_results(
            &request.delegation_id,
            results,
            None,
        ))
    }

    /// Adversarial review: producer creates, reviewer critiques, repeat.
    async fn execute_adversarial(
        &self,
        request: &DelegationRequest,
        producer_id: &str,
        reviewer_id: &str,
        max_rounds: u32,
    ) -> Result<DelegationResult, String> {
        let reg = self.registry.read().await;
        let mut results = Vec::new();
        let mut last_producer_output: Option<String> = None;

        let producer_profile = reg
            .get(producer_id)
            .cloned()
            .unwrap_or_else(|| AgentProfile::new(producer_id, producer_id, mo_agent_services::coordination::AgentTier::System));
        let reviewer_profile = reg
            .get(reviewer_id)
            .cloned()
            .unwrap_or_else(|| AgentProfile::new(reviewer_id, reviewer_id, mo_agent_services::coordination::AgentTier::System));
        drop(reg);

        for round in 0..max_rounds {
            // ── Producer sub-run ──
            let prod_run_id = uuid::Uuid::new_v4().to_string();
            self.run_engine
                .start_run(&prod_run_id, &request.user_id, &prod_run_id)
                .await?;
            self.tracker
                .record_sub_run(SubRunRecord {
                    run_id: prod_run_id.clone(),
                    parent_run_id: request.parent_run_id.clone(),
                    delegation_id: request.delegation_id.clone(),
                    agent_id: producer_id.to_string(),
                    depth: request.depth + 1,
                })
                .await;
            self.run_engine
                .persist_status(&prod_run_id, "running", Some("produce"), None)
                .await?;
            self.run_engine
                .append_event(
                    &prod_run_id,
                    serde_json::json!({"event_type": "adversarial_round", "data": {"round": round, "role": "producer"}}),
                )
                .await?;

            let prod_config = SubRunConfig {
                run_id: prod_run_id.clone(),
                agent_profile: producer_profile.clone(),
                task: request.task.clone(),
                session_id: request.context.get("session_id").and_then(|v| v.as_str()).unwrap_or("delegation").to_string(),
                user_id: request.user_id.clone(),
                previous_output: last_producer_output.clone(),
                context: request.context.clone(),
            };
            let prod_result = match self.executor.execute(prod_config).await {
                Ok(r) => {
                    let _ = self.run_engine.persist_status(&prod_run_id, &r.status, None, r.error.as_deref()).await;
                    r
                }
                Err(e) => {
                    let _ = self.run_engine.persist_status(&prod_run_id, "failed", None, Some(&e)).await;
                    AgentResult {
                        agent_id: producer_id.to_string(), run_id: prod_run_id,
                        status: "failed".to_string(), output: None, error: Some(e),
                        prompt_tokens: 0, completion_tokens: 0, tool_calls: 0,
                    }
                }
            };
            last_producer_output = prod_result.output.clone();
            results.push(prod_result);

            // ── Reviewer sub-run ──
            let rev_run_id = uuid::Uuid::new_v4().to_string();
            self.run_engine
                .start_run(&rev_run_id, &request.user_id, &rev_run_id)
                .await?;
            self.tracker
                .record_sub_run(SubRunRecord {
                    run_id: rev_run_id.clone(),
                    parent_run_id: request.parent_run_id.clone(),
                    delegation_id: request.delegation_id.clone(),
                    agent_id: reviewer_id.to_string(),
                    depth: request.depth + 1,
                })
                .await;
            self.run_engine
                .persist_status(&rev_run_id, "running", Some("review"), None)
                .await?;
            self.run_engine
                .append_event(
                    &rev_run_id,
                    serde_json::json!({"event_type": "adversarial_round", "data": {"round": round, "role": "reviewer"}}),
                )
                .await?;

            let rev_config = SubRunConfig {
                run_id: rev_run_id.clone(),
                agent_profile: reviewer_profile.clone(),
                task: format!("Review this output:\n\n{}", last_producer_output.as_deref().unwrap_or("[no output]")),
                session_id: request.context.get("session_id").and_then(|v| v.as_str()).unwrap_or("delegation").to_string(),
                user_id: request.user_id.clone(),
                previous_output: last_producer_output.clone(),
                context: request.context.clone(),
            };
            let rev_result = match self.executor.execute(rev_config).await {
                Ok(r) => {
                    let _ = self.run_engine.persist_status(&rev_run_id, &r.status, None, r.error.as_deref()).await;
                    r
                }
                Err(e) => {
                    let _ = self.run_engine.persist_status(&rev_run_id, "failed", None, Some(&e)).await;
                    AgentResult {
                        agent_id: reviewer_id.to_string(), run_id: rev_run_id,
                        status: "failed".to_string(), output: None, error: Some(e),
                        prompt_tokens: 0, completion_tokens: 0, tool_calls: 0,
                    }
                }
            };
            results.push(rev_result);
        }

        Ok(DelegationResult::from_results(
            &request.delegation_id,
            results,
            None,
        ))
    }

    /// Get the delegation tracker for external queries.
    pub fn tracker(&self) -> &Arc<DelegationTracker> {
        &self.tracker
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use mo_agent_services::coordination::{AgentProfile, AgentTier, PipelineStage};
    use mo_agent_services::runs::InMemoryRunStateStore;

    fn setup() -> (
        Arc<RwLock<AgentProfileRegistry>>,
        Arc<RunEngine>,
        Arc<DelegationTracker>,
    ) {
        let mut reg = AgentProfileRegistry::new();
        reg.register(AgentProfile::new(
            "orch",
            "Orchestrator",
            AgentTier::Orchestrator,
        ))
        .unwrap();
        reg.register(AgentProfile::new("coder", "Coder", AgentTier::System))
            .unwrap();
        reg.register(AgentProfile::new("reviewer", "Reviewer", AgentTier::System))
            .unwrap();
        reg.register(AgentProfile::new("writer", "Writer", AgentTier::User))
            .unwrap();

        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = Arc::new(RunEngine::new(store));
        let tracker = Arc::new(DelegationTracker::new());

        (Arc::new(RwLock::new(reg)), engine, tracker)
    }

    fn fan_out_request(agents: Vec<&str>) -> DelegationRequest {
        DelegationRequest {
            delegation_id: "del-1".into(),
            parent_run_id: "parent-1".into(),
            task: "test task".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: agents.into_iter().map(String::from).collect(),
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 60,
            },
            user_id: "user-1".into(),
            depth: 0,
            context: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn delegation_tracker_records_and_queries() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "sub-1".into(),
                parent_run_id: "parent-1".into(),
                delegation_id: "del-1".into(),
                agent_id: "coder".into(),
                depth: 1,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "sub-2".into(),
                parent_run_id: "parent-1".into(),
                delegation_id: "del-1".into(),
                agent_id: "reviewer".into(),
                depth: 1,
            })
            .await;

        let subs = tracker.get_sub_runs("del-1").await;
        assert_eq!(subs.len(), 2);
        assert!(tracker.is_sub_run("sub-1").await);
        assert!(!tracker.is_sub_run("parent-1").await);
        assert_eq!(
            tracker.get_parent("sub-1").await.as_deref(),
            Some("parent-1")
        );
    }

    #[tokio::test]
    async fn delegation_tracker_ancestry() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "child".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "a".into(),
                depth: 1,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "grandchild".into(),
                parent_run_id: "child".into(),
                delegation_id: "d2".into(),
                agent_id: "b".into(),
                depth: 2,
            })
            .await;

        let ancestry = tracker.get_ancestry("grandchild").await;
        assert_eq!(ancestry, vec!["child", "parent"]);
    }

    #[tokio::test]
    async fn fan_out_spawns_sub_runs() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::new(reg, engine.clone(), tracker.clone());

        let req = fan_out_request(vec!["coder", "reviewer"]);
        let result = de.execute(req, "orch").await.unwrap();

        assert_eq!(result.agent_results.len(), 2);
        assert_eq!(result.delegation_id, "del-1");
        // Stub executor marks runs as completed
        assert_eq!(result.status, "completed");

        // Verify sub-runs were created in engine with final status
        for ar in &result.agent_results {
            assert_eq!(ar.status, "completed");
            assert!(ar.output.is_some());
            let run = engine.load_run(&ar.run_id).await.unwrap().unwrap();
            assert_eq!(run.status, "completed");
        }

        // Verify tracker has the records
        let subs = tracker.get_sub_runs("del-1").await;
        assert_eq!(subs.len(), 2);
        assert!(subs.iter().all(|s| s.parent_run_id == "parent-1"));
        assert!(subs.iter().all(|s| s.depth == 1));
    }

    #[tokio::test]
    async fn sequential_spawns_ordered_sub_runs() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::new(reg, engine.clone(), tracker.clone());

        let req = DelegationRequest {
            delegation_id: "del-seq".into(),
            parent_run_id: "parent-2".into(),
            task: "sequential test".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into(), "reviewer".into()],
                stop_on_success: false,
            },
            user_id: "user-1".into(),
            depth: 0,
            context: HashMap::new(),
        };

        let result = de.execute(req, "orch").await.unwrap();
        assert_eq!(result.agent_results.len(), 2);
        assert_eq!(result.agent_results[0].agent_id, "coder");
        assert_eq!(result.agent_results[1].agent_id, "reviewer");
    }

    #[tokio::test]
    async fn pipeline_spawns_stage_runs() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::new(reg, engine.clone(), tracker.clone());

        let req = DelegationRequest {
            delegation_id: "del-pipe".into(),
            parent_run_id: "parent-3".into(),
            task: "pipeline test".into(),
            pattern: CoordinationPattern::Pipeline {
                stages: vec![
                    PipelineStage {
                        agent_id: "coder".into(),
                        output_transform: None,
                    },
                    PipelineStage {
                        agent_id: "reviewer".into(),
                        output_transform: Some("extract_issues".into()),
                    },
                ],
            },
            user_id: "user-1".into(),
            depth: 0,
            context: HashMap::new(),
        };

        let result = de.execute(req, "orch").await.unwrap();
        assert_eq!(result.agent_results.len(), 2);

        let subs = tracker.get_sub_runs("del-pipe").await;
        assert_eq!(subs.len(), 2);
    }

    #[tokio::test]
    async fn adversarial_spawns_producer_reviewer_pairs() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::new(reg, engine.clone(), tracker.clone());

        let req = DelegationRequest {
            delegation_id: "del-adv".into(),
            parent_run_id: "parent-4".into(),
            task: "adversarial test".into(),
            pattern: CoordinationPattern::AdversarialReview {
                producer_id: "coder".into(),
                reviewer_id: "reviewer".into(),
                max_rounds: 2,
                acceptance_threshold: 0.8,
            },
            user_id: "user-1".into(),
            depth: 0,
            context: HashMap::new(),
        };

        let result = de.execute(req, "orch").await.unwrap();
        // 2 rounds × 2 agents = 4 sub-runs
        assert_eq!(result.agent_results.len(), 4);

        let subs = tracker.get_sub_runs("del-adv").await;
        assert_eq!(subs.len(), 4);

        // Verify alternating producer/reviewer
        assert_eq!(subs[0].agent_id, "coder");
        assert_eq!(subs[1].agent_id, "reviewer");
        assert_eq!(subs[2].agent_id, "coder");
        assert_eq!(subs[3].agent_id, "reviewer");
    }

    #[tokio::test]
    async fn validation_rejects_bad_delegation() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::new(reg, engine, tracker);

        // User agent cannot delegate
        let req = DelegationRequest {
            delegation_id: "bad".into(),
            parent_run_id: "p".into(),
            task: "fail".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: true,
            },
            user_id: "u".into(),
            depth: 0,
            context: HashMap::new(),
        };

        assert!(de.execute(req, "writer").await.is_err());
    }

    #[tokio::test]
    async fn depth_limit_enforcement() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::new(reg, engine, tracker);

        // Orchestrator max depth is 3; request at depth=5 should fail
        let req = DelegationRequest {
            delegation_id: "deep".into(),
            parent_run_id: "p".into(),
            task: "too deep".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: true,
            },
            user_id: "u".into(),
            depth: 5,
            context: HashMap::new(),
        };

        let err = de.execute(req, "orch").await.unwrap_err();
        assert!(err.contains("depth"));
    }

    #[tokio::test]
    async fn cross_delegation_isolation() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::new(reg, engine.clone(), tracker.clone());

        let req1 = DelegationRequest {
            delegation_id: "del-A".into(),
            parent_run_id: "pA".into(),
            task: "a".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["coder".into()],
                aggregation: AggregationStrategy::FirstSuccess,
                timeout_sec: 60,
            },
            user_id: "u".into(),
            depth: 0,
            context: HashMap::new(),
        };
        let req2 = DelegationRequest {
            delegation_id: "del-B".into(),
            parent_run_id: "pB".into(),
            task: "b".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["reviewer".into()],
                aggregation: AggregationStrategy::FirstSuccess,
                timeout_sec: 60,
            },
            user_id: "u".into(),
            depth: 0,
            context: HashMap::new(),
        };

        de.execute(req1, "orch").await.unwrap();
        de.execute(req2, "orch").await.unwrap();

        let subs_a = tracker.get_sub_runs("del-A").await;
        let subs_b = tracker.get_sub_runs("del-B").await;
        assert_eq!(subs_a.len(), 1);
        assert_eq!(subs_b.len(), 1);
        assert_eq!(subs_a[0].agent_id, "coder");
        assert_eq!(subs_b[0].agent_id, "reviewer");
    }

    // ─── Custom executor for testing ────────────────────────────────────────

    /// Test executor that echoes the task back with agent_id prefix.
    struct EchoExecutor;

    #[async_trait]
    impl SubRunExecutor for EchoExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            let output = if let Some(prev) = &config.previous_output {
                format!("[{}] {}: prev={}", config.agent_profile.agent_id, config.task, prev)
            } else {
                format!("[{}] {}", config.agent_profile.agent_id, config.task)
            };
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id,
                run_id: config.run_id,
                status: "completed".to_string(),
                output: Some(output),
                error: None,
                prompt_tokens: 10,
                completion_tokens: 20,
                tool_calls: 1,
            })
        }
    }

    /// Test executor that fails for specific agents.
    struct FailingExecutor {
        fail_agents: Vec<String>,
    }

    #[async_trait]
    impl SubRunExecutor for FailingExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            if self.fail_agents.contains(&config.agent_profile.agent_id) {
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: "failed".to_string(),
                    output: None,
                    error: Some("intentional test failure".to_string()),
                    prompt_tokens: 5,
                    completion_tokens: 0,
                    tool_calls: 0,
                })
            } else {
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id.clone(),
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some(format!("[{}] done", config.agent_profile.agent_id)),
                    error: None,
                    prompt_tokens: 10,
                    completion_tokens: 20,
                    tool_calls: 1,
                })
            }
        }
    }

    fn setup_with_executor(executor: Arc<dyn SubRunExecutor>) -> (
        Arc<RwLock<AgentProfileRegistry>>,
        Arc<RunEngine>,
        Arc<DelegationTracker>,
        DelegationEngine,
    ) {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(
            reg.clone(), engine.clone(), tracker.clone(), executor,
        );
        (reg, engine, tracker, de)
    }

    #[tokio::test]
    async fn fan_out_executes_with_custom_executor() {
        let (_, engine, tracker, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = fan_out_request(vec!["coder", "reviewer"]);
        let result = de.execute(req, "orch").await.unwrap();

        assert_eq!(result.status, "completed");
        assert_eq!(result.agent_results.len(), 2);

        // Both agents should have executed and produced output
        for ar in &result.agent_results {
            assert_eq!(ar.status, "completed");
            assert!(ar.output.as_ref().unwrap().contains("test task"));
            assert_eq!(ar.prompt_tokens, 10);
            assert_eq!(ar.completion_tokens, 20);
            assert_eq!(ar.tool_calls, 1);
        }

        // Token aggregation
        assert_eq!(result.total_prompt_tokens, 20);
        assert_eq!(result.total_completion_tokens, 40);
        assert_eq!(result.total_tool_calls, 2);

        // Engine persisted final status
        for ar in &result.agent_results {
            let run = engine.load_run(&ar.run_id).await.unwrap().unwrap();
            assert_eq!(run.status, "completed");
        }

        // Tracker recorded hierarchy
        let subs = tracker.get_sub_runs("del-1").await;
        assert_eq!(subs.len(), 2);
    }

    #[tokio::test]
    async fn sequential_passes_output_to_next_stage() {
        let (_, _, _, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = DelegationRequest {
            delegation_id: "del-pipe".into(),
            parent_run_id: "p".into(),
            task: "build code".into(),
            pattern: CoordinationPattern::Pipeline {
                stages: vec![
                    PipelineStage {
                        agent_id: "coder".into(),
                        output_transform: None,
                    },
                    PipelineStage {
                        agent_id: "reviewer".into(),
                        output_transform: None,
                    },
                ],
            },
            user_id: "u".into(),
            depth: 0,
            context: HashMap::new(),
        };

        let result = de.execute(req, "orch").await.unwrap();
        assert_eq!(result.agent_results.len(), 2);

        // First stage has no previous_output
        let first = &result.agent_results[0];
        assert_eq!(first.agent_id, "coder");
        assert!(!first.output.as_ref().unwrap().contains("prev="));

        // Second stage receives first stage's output
        let second = &result.agent_results[1];
        assert_eq!(second.agent_id, "reviewer");
        assert!(second.output.as_ref().unwrap().contains("prev="));
        assert!(second.output.as_ref().unwrap().contains("[coder]"));
    }

    #[tokio::test]
    async fn sequential_stop_on_success_stops_early() {
        let (_, _, _, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = DelegationRequest {
            delegation_id: "del-early".into(),
            parent_run_id: "p".into(),
            task: "find answer".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into(), "reviewer".into(), "writer".into()],
                stop_on_success: true,
            },
            user_id: "u".into(),
            depth: 0,
            context: HashMap::new(),
        };

        let result = de.execute(req, "orch").await.unwrap();
        // First agent succeeds → stops
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].agent_id, "coder");
    }

    #[tokio::test]
    async fn fan_out_partial_failure() {
        let executor = Arc::new(FailingExecutor {
            fail_agents: vec!["reviewer".to_string()],
        });
        let (_, _, _, de) = setup_with_executor(executor);

        let req = fan_out_request(vec!["coder", "reviewer"]);
        let result = de.execute(req, "orch").await.unwrap();

        assert_eq!(result.status, "partial");
        assert_eq!(result.agent_results.len(), 2);

        let coder = result.agent_results.iter().find(|r| r.agent_id == "coder").unwrap();
        assert_eq!(coder.status, "completed");
        assert!(coder.output.is_some());

        let reviewer = result.agent_results.iter().find(|r| r.agent_id == "reviewer").unwrap();
        assert_eq!(reviewer.status, "failed");
        assert!(reviewer.error.is_some());
    }

    #[tokio::test]
    async fn adversarial_executes_all_rounds() {
        let (_, _, tracker, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = DelegationRequest {
            delegation_id: "del-adv".into(),
            parent_run_id: "p".into(),
            task: "write code".into(),
            pattern: CoordinationPattern::AdversarialReview {
                producer_id: "coder".into(),
                reviewer_id: "reviewer".into(),
                max_rounds: 2,
                acceptance_threshold: 0.8,
            },
            user_id: "u".into(),
            depth: 0,
            context: HashMap::new(),
        };

        let result = de.execute(req, "orch").await.unwrap();
        // 2 rounds × (producer + reviewer) = 4
        assert_eq!(result.agent_results.len(), 4);
        assert_eq!(result.status, "completed");

        // All agents produced output
        for ar in &result.agent_results {
            assert!(ar.output.is_some());
        }

        // Token aggregation across all sub-runs
        assert_eq!(result.total_prompt_tokens, 40);  // 4 × 10
        assert_eq!(result.total_completion_tokens, 80); // 4 × 20

        let subs = tracker.get_sub_runs("del-adv").await;
        assert_eq!(subs.len(), 4);
    }

    #[tokio::test]
    async fn tracker_get_agent_id_returns_correct_id() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "sub-1".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "coder".into(),
                depth: 1,
            })
            .await;

        assert_eq!(tracker.get_agent_id("sub-1").await, Some("coder".to_string()));
        assert_eq!(tracker.get_agent_id("parent").await, None);
        assert_eq!(tracker.get_agent_id("nonexistent").await, None);
    }

    #[tokio::test]
    async fn with_executor_constructor_uses_custom_executor() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(
            reg, engine, tracker,
            Arc::new(EchoExecutor),
        );

        let req = fan_out_request(vec!["coder"]);
        let result = de.execute(req, "orch").await.unwrap();
        assert_eq!(result.status, "completed");
        // EchoExecutor returns prompt_tokens=10
        assert_eq!(result.total_prompt_tokens, 10);
    }

    #[tokio::test]
    async fn sub_run_config_passes_context() {
        /// Executor that checks context is passed through.
        struct ContextCheckExecutor;

        #[async_trait]
        impl SubRunExecutor for ContextCheckExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
                let has_key = config.context.contains_key("test_key");
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some(format!("context_present={}", has_key)),
                    error: None,
                    prompt_tokens: 0, completion_tokens: 0, tool_calls: 0,
                })
            }
        }

        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(
            reg, engine, tracker, Arc::new(ContextCheckExecutor),
        );

        let mut ctx = HashMap::new();
        ctx.insert("test_key".to_string(), serde_json::json!("test_value"));

        let req = DelegationRequest {
            delegation_id: "ctx-test".into(),
            parent_run_id: "p".into(),
            task: "check context".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
            },
            user_id: "u".into(),
            depth: 0,
            context: ctx,
        };

        let result = de.execute(req, "orch").await.unwrap();
        assert_eq!(
            result.agent_results[0].output.as_deref(),
            Some("context_present=true")
        );
    }

    #[tokio::test]
    async fn stub_executor_returns_completed() {
        let executor = StubSubRunExecutor;
        let config = SubRunConfig {
            run_id: "r1".into(),
            agent_profile: AgentProfile::new("test", "Test", AgentTier::User),
            task: "hello world".into(),
            session_id: "s1".into(),
            user_id: "u1".into(),
            previous_output: None,
            context: HashMap::new(),
        };

        let result = executor.execute(config).await.unwrap();
        assert_eq!(result.status, "completed");
        assert!(result.output.unwrap().contains("hello world"));
    }
}
