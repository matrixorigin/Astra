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
//!   │     ├── sub-run-B (agent s1)  ──▶ completed
//!   │     └── sub-run-C (agent s2)  ──▶ completed
//!   │
//!   └── aggregate(results) ──▶ merged output
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use mo_agent_services::coordination::{
    AgentProfileRegistry, AgentResult, AggregationStrategy, CoordinationPattern,
    DelegationRequest, DelegationResult, aggregate_results,
};

use super::run_engine::RunEngine;

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
/// and aggregates results.
pub struct DelegationEngine {
    /// Agent profiles for validation.
    registry: Arc<RwLock<AgentProfileRegistry>>,
    /// Run engine for spawning sub-runs.
    run_engine: Arc<RunEngine>,
    /// Tracks parent→child run relationships.
    tracker: Arc<DelegationTracker>,
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
        }
    }

    /// Validate a delegation request without executing it.
    pub async fn validate(&self, request: &DelegationRequest, source_agent_id: &str) -> Result<(), String> {
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
            } => {
                self.execute_fan_out(&request, agent_ids, aggregation)
                    .await
            }
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
        let mut results = Vec::new();

        for agent_id in agent_ids {
            let sub_run_id = uuid::Uuid::new_v4().to_string();

            // Create the sub-run in the durable store
            self.run_engine
                .start_run(&sub_run_id, &request.user_id, &sub_run_id)
                .await?;

            // Track the hierarchy
            self.tracker
                .record_sub_run(SubRunRecord {
                    run_id: sub_run_id.clone(),
                    parent_run_id: request.parent_run_id.clone(),
                    delegation_id: request.delegation_id.clone(),
                    agent_id: agent_id.clone(),
                    depth: request.depth + 1,
                })
                .await;

            // Mark as completed (actual execution is handled by the agentic loop
            // when the server wires this up — here we record the structural intent)
            self.run_engine
                .persist_status(&sub_run_id, "waiting", Some("agent_execution"), None)
                .await?;

            results.push(AgentResult {
                agent_id: agent_id.clone(),
                run_id: sub_run_id,
                status: "waiting".to_string(),
                output: None,
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            });
        }

        let aggregated = aggregate_results(aggregation, &results);
        Ok(DelegationResult::from_results(
            &request.delegation_id,
            results,
            aggregated,
        ))
    }

    /// Sequential / Pipeline: execute agents one after another.
    async fn execute_sequential(
        &self,
        request: &DelegationRequest,
        agent_ids: &[String],
        stop_on_success: bool,
    ) -> Result<DelegationResult, String> {
        let mut results = Vec::new();

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
                .persist_status(&sub_run_id, "waiting", Some("agent_execution"), None)
                .await?;

            let result = AgentResult {
                agent_id: agent_id.clone(),
                run_id: sub_run_id,
                status: "waiting".to_string(),
                output: None,
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            };

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
        let mut results = Vec::new();

        for round in 0..max_rounds {
            // Producer sub-run
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
                .persist_status(&prod_run_id, "waiting", Some("produce"), None)
                .await?;
            self.run_engine
                .append_event(
                    &prod_run_id,
                    serde_json::json!({"event_type": "adversarial_round", "data": {"round": round, "role": "producer"}}),
                )
                .await?;
            results.push(AgentResult {
                agent_id: producer_id.to_string(),
                run_id: prod_run_id,
                status: "waiting".to_string(),
                output: None,
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            });

            // Reviewer sub-run
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
                .persist_status(&rev_run_id, "waiting", Some("review"), None)
                .await?;
            self.run_engine
                .append_event(
                    &rev_run_id,
                    serde_json::json!({"event_type": "adversarial_round", "data": {"round": round, "role": "reviewer"}}),
                )
                .await?;
            results.push(AgentResult {
                agent_id: reviewer_id.to_string(),
                run_id: rev_run_id,
                status: "waiting".to_string(),
                output: None,
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            });
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
        reg.register(AgentProfile::new("orch", "Orchestrator", AgentTier::Orchestrator)).unwrap();
        reg.register(AgentProfile::new("coder", "Coder", AgentTier::System)).unwrap();
        reg.register(AgentProfile::new("reviewer", "Reviewer", AgentTier::System)).unwrap();
        reg.register(AgentProfile::new("writer", "Writer", AgentTier::User)).unwrap();

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

        // Verify sub-runs were created in engine
        for ar in &result.agent_results {
            let run = engine.load_run(&ar.run_id).await.unwrap().unwrap();
            assert_eq!(run.status, "waiting");
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
}
