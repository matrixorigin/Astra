//! Multi-agent coordination framework.
//!
//! Provides agent profiles with tier-based permissions, coordination patterns
//! for multi-agent task execution, delegation engine for spawning sub-runs,
//! and result aggregation strategies.
//!
//! # Agent Tier Hierarchy
//!
//! ```text
//! ┌──────────────┐    can delegate to
//! │ ORCHESTRATOR │───────────────────┐
//! │  (tier 0)    │                   │
//! └──────┬───────┘                   │
//!        │ can delegate to           │
//! ┌──────▼───────┐                   │
//! │   SYSTEM     │◄──────────────────┘
//! │  (tier 1)    │
//! └──────┬───────┘
//!        │ can delegate to
//! ┌──────▼───────┐
//! │    USER      │
//! │  (tier 2)    │
//! └──────────────┘
//! ```
//!
//! # Coordination Patterns
//!
//! - **FanOut**: Dispatch task to N agents in parallel, aggregate results
//! - **Pipeline**: Sequential chain where each agent's output feeds the next
//! - **AdversarialReview**: One agent produces, another reviews/critiques
//! - **Sequential**: Simple sequential delegation to agents in order

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─── Agent Profile ──────────────────────────────────────────────────────────

/// Agent capability tier determining delegation permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTier {
    /// Top-level orchestrator: can delegate to all tiers.
    Orchestrator,
    /// System agent: can delegate to User agents.
    System,
    /// User-facing agent: cannot delegate further.
    User,
}

impl AgentTier {
    /// Numeric rank (lower = more privileged).
    pub fn rank(self) -> u8 {
        match self {
            Self::Orchestrator => 0,
            Self::System => 1,
            Self::User => 2,
        }
    }

    /// Whether this tier can delegate to the target tier.
    pub fn can_delegate_to(self, target: AgentTier) -> bool {
        self.rank() < target.rank()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Orchestrator => "orchestrator",
            Self::System => "system",
            Self::User => "user",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "orchestrator" => Self::Orchestrator,
            "system" => Self::System,
            _ => Self::User,
        }
    }
}

/// Extended profile for an agent participating in multi-agent coordination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProfile {
    /// Unique agent identifier.
    pub agent_id: String,
    /// Human-readable name.
    pub name: String,
    /// Tier determining delegation permissions.
    pub tier: AgentTier,
    /// Optional system prompt override for this agent.
    pub system_prompt: Option<String>,
    /// Filter limiting which skills/tools this agent can use.
    /// Empty = unrestricted.
    pub skill_filter: Vec<String>,
    /// Model override (e.g., "gpt-4o", "claude-3-opus").
    pub model_override: Option<String>,
    /// Whether this agent can delegate tasks to sub-agents.
    pub can_delegate: bool,
    /// Explicit list of agent IDs this agent may delegate to.
    /// Empty = any agent at a lower tier.
    pub delegate_to: Vec<String>,
    /// Maximum delegation depth (prevents infinite loops).
    pub max_delegation_depth: u32,
    /// Optional triggers that auto-activate this agent.
    pub triggers: Vec<AgentTrigger>,
    /// Additional metadata.
    pub metadata: HashMap<String, serde_json::Value>,
}

impl AgentProfile {
    pub fn new(agent_id: &str, name: &str, tier: AgentTier) -> Self {
        Self {
            agent_id: agent_id.to_string(),
            name: name.to_string(),
            tier,
            system_prompt: None,
            skill_filter: Vec::new(),
            model_override: None,
            can_delegate: tier != AgentTier::User,
            delegate_to: Vec::new(),
            max_delegation_depth: match tier {
                AgentTier::Orchestrator => 3,
                AgentTier::System => 1,
                AgentTier::User => 0,
            },
            triggers: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    /// Check if this agent can delegate to a specific target agent.
    pub fn can_delegate_to_agent(&self, target: &AgentProfile) -> bool {
        if !self.can_delegate {
            return false;
        }
        if !self.tier.can_delegate_to(target.tier) {
            return false;
        }
        if !self.delegate_to.is_empty() && !self.delegate_to.contains(&target.agent_id) {
            return false;
        }
        true
    }
}

/// Trigger that can auto-activate an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTrigger {
    /// Trigger type (e.g., "keyword", "tool_failure", "plan_step").
    pub trigger_type: String,
    /// Trigger-specific pattern or condition.
    pub pattern: String,
}

// ─── Coordination Patterns ──────────────────────────────────────────────────

/// Pattern for coordinating multiple agents on a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "pattern", rename_all = "snake_case")]
pub enum CoordinationPattern {
    /// Dispatch to N agents in parallel, aggregate results.
    FanOut {
        /// Agent IDs to dispatch to.
        agent_ids: Vec<String>,
        /// How to aggregate results.
        aggregation: AggregationStrategy,
        /// Maximum time per agent (seconds).
        timeout_sec: u64,
    },

    /// Sequential chain: output of agent N feeds into agent N+1.
    Pipeline {
        /// Ordered list of agent IDs forming the pipeline.
        stages: Vec<PipelineStage>,
    },

    /// One agent produces, another reviews.
    AdversarialReview {
        /// The producing agent.
        producer_id: String,
        /// The reviewing agent.
        reviewer_id: String,
        /// Maximum revision rounds.
        max_rounds: u32,
        /// Minimum acceptance confidence (0.0-1.0).
        acceptance_threshold: f64,
    },

    /// Simple sequential delegation.
    Sequential {
        /// Ordered agent IDs.
        agent_ids: Vec<String>,
        /// Stop on first success?
        stop_on_success: bool,
    },
}

/// A stage in a pipeline coordination pattern.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineStage {
    /// Agent to execute this stage.
    pub agent_id: String,
    /// Transformation to apply to output before passing to next stage.
    /// `None` = pass full output.
    pub output_transform: Option<String>,
}

/// Strategy for aggregating results from multiple agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggregationStrategy {
    /// Use the first successful result.
    FirstSuccess,
    /// Collect all results.
    AllResults,
    /// Majority/consensus among results.
    Consensus,
    /// Use an LLM to synthesize a merged result.
    LlmGuided { prompt_template: String },
}

// ─── Delegation Request / Result ────────────────────────────────────────────

/// Request to delegate a task to one or more agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationRequest {
    /// Unique delegation ID.
    pub delegation_id: String,
    /// Parent run ID (for hierarchy tracking).
    pub parent_run_id: String,
    /// Task description/prompt for the delegated agents.
    pub task: String,
    /// Coordination pattern to use.
    pub pattern: CoordinationPattern,
    /// User ID owning this delegation.
    pub user_id: String,
    /// Current delegation depth (for loop prevention).
    pub depth: u32,
    /// Context to pass to delegated agents.
    pub context: HashMap<String, serde_json::Value>,
}

/// Result from a single agent's execution within a delegation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResult {
    /// Agent that produced this result.
    pub agent_id: String,
    /// Run ID for this agent's execution.
    pub run_id: String,
    /// Status: "completed", "failed", "timeout".
    pub status: String,
    /// The agent's output/response.
    pub output: Option<String>,
    /// Error message if failed.
    pub error: Option<String>,
    /// Token usage.
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub tool_calls: u32,
}

impl AgentResult {
    pub fn is_success(&self) -> bool {
        self.status == "completed" && self.error.is_none()
    }
}

/// Aggregated result from a coordination pattern.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationResult {
    /// Delegation ID.
    pub delegation_id: String,
    /// Overall status.
    pub status: String,
    /// Individual agent results.
    pub agent_results: Vec<AgentResult>,
    /// The synthesized/aggregated final output.
    pub aggregated_output: Option<String>,
    /// Total tokens across all agents.
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_tool_calls: u32,
}

impl DelegationResult {
    /// Aggregate token counts from agent results.
    pub fn from_results(
        delegation_id: &str,
        results: Vec<AgentResult>,
        aggregated_output: Option<String>,
    ) -> Self {
        let total_prompt: u64 = results.iter().map(|r| r.prompt_tokens).sum();
        let total_completion: u64 = results.iter().map(|r| r.completion_tokens).sum();
        let total_tools: u32 = results.iter().map(|r| r.tool_calls).sum();
        let all_ok = results.iter().all(|r| r.is_success());
        let any_ok = results.iter().any(|r| r.is_success());

        Self {
            delegation_id: delegation_id.to_string(),
            status: if all_ok {
                "completed".to_string()
            } else if any_ok {
                "partial".to_string()
            } else {
                "failed".to_string()
            },
            agent_results: results,
            aggregated_output,
            total_prompt_tokens: total_prompt,
            total_completion_tokens: total_completion,
            total_tool_calls: total_tools,
        }
    }
}

// ─── Agent Profile Registry ─────────────────────────────────────────────────

/// In-memory agent profile registry for coordination.
///
/// Production deployments extend this with database-backed storage via
/// the existing `AgentService` (agents.rs).
#[derive(Clone)]
pub struct AgentProfileRegistry {
    profiles: HashMap<String, AgentProfile>,
}

impl AgentProfileRegistry {
    pub fn new() -> Self {
        Self {
            profiles: HashMap::new(),
        }
    }

    /// Register or update a profile.
    pub fn register(&mut self, profile: AgentProfile) -> Result<(), String> {
        self.profiles.insert(profile.agent_id.clone(), profile);
        Ok(())
    }

    /// Get a profile by ID.
    pub fn get(&self, agent_id: &str) -> Option<&AgentProfile> {
        self.profiles.get(agent_id)
    }

    /// Remove a profile.
    pub fn remove(&mut self, agent_id: &str) -> Option<AgentProfile> {
        self.profiles.remove(agent_id)
    }

    /// List all profiles.
    pub fn list(&self) -> Vec<&AgentProfile> {
        self.profiles.values().collect()
    }

    /// List profiles by tier.
    pub fn list_by_tier(&self, tier: AgentTier) -> Vec<&AgentProfile> {
        self.profiles.values().filter(|p| p.tier == tier).collect()
    }

    /// Find agents that can handle a delegation from the source agent.
    pub fn find_delegates(&self, source: &AgentProfile) -> Vec<&AgentProfile> {
        self.profiles
            .values()
            .filter(|target| source.can_delegate_to_agent(target))
            .collect()
    }

    /// Validate a delegation request against the registry.
    pub fn validate_delegation(
        &self,
        request: &DelegationRequest,
        source_agent_id: &str,
    ) -> Result<(), String> {
        let source = self
            .get(source_agent_id)
            .ok_or_else(|| format!("source agent '{}' not registered", source_agent_id))?;

        if !source.can_delegate {
            return Err(format!(
                "agent '{}' ({}) cannot delegate",
                source.name,
                source.tier.as_str()
            ));
        }

        if request.depth >= source.max_delegation_depth {
            return Err(format!(
                "delegation depth {} exceeds max {} for agent '{}'",
                request.depth, source.max_delegation_depth, source.name
            ));
        }

        let agent_ids = match &request.pattern {
            CoordinationPattern::FanOut { agent_ids, .. } => agent_ids.clone(),
            CoordinationPattern::Pipeline { stages } => {
                stages.iter().map(|s| s.agent_id.clone()).collect()
            }
            CoordinationPattern::AdversarialReview {
                producer_id,
                reviewer_id,
                ..
            } => vec![producer_id.clone(), reviewer_id.clone()],
            CoordinationPattern::Sequential { agent_ids, .. } => agent_ids.clone(),
        };

        for target_id in &agent_ids {
            let target = self
                .get(target_id)
                .ok_or_else(|| format!("target agent '{}' not registered", target_id))?;
            if !source.can_delegate_to_agent(target) {
                return Err(format!(
                    "agent '{}' ({}) cannot delegate to '{}' ({})",
                    source.name,
                    source.tier.as_str(),
                    target.name,
                    target.tier.as_str()
                ));
            }
        }

        Ok(())
    }
}

impl Default for AgentProfileRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Result Aggregator ──────────────────────────────────────────────────────

/// Aggregate results from multiple agents according to a strategy.
pub fn aggregate_results(
    strategy: &AggregationStrategy,
    results: &[AgentResult],
) -> Option<String> {
    match strategy {
        AggregationStrategy::FirstSuccess => results
            .iter()
            .find(|r| r.is_success())
            .and_then(|r| r.output.clone()),

        AggregationStrategy::AllResults => {
            let outputs: Vec<String> = results
                .iter()
                .filter(|r| r.is_success())
                .filter_map(|r| r.output.clone())
                .collect();
            if outputs.is_empty() {
                None
            } else {
                Some(outputs.join("\n---\n"))
            }
        }

        AggregationStrategy::Consensus => {
            // Simple majority: find the most common output
            let mut counts: HashMap<&str, usize> = HashMap::new();
            for r in results.iter().filter(|r| r.is_success()) {
                if let Some(ref out) = r.output {
                    *counts.entry(out.as_str()).or_default() += 1;
                }
            }
            counts
                .into_iter()
                .max_by_key(|(_, count)| *count)
                .map(|(output, _)| output.to_string())
        }

        AggregationStrategy::LlmGuided { .. } => {
            // LLM-guided aggregation requires an LLM call — return all outputs
            // formatted for the caller to pass to an LLM.
            let outputs: Vec<String> = results
                .iter()
                .filter(|r| r.is_success())
                .enumerate()
                .filter_map(|(i, r)| {
                    r.output
                        .as_ref()
                        .map(|o| format!("Agent {} ({}):\n{}", i + 1, r.agent_id, o))
                })
                .collect();
            if outputs.is_empty() {
                None
            } else {
                Some(outputs.join("\n\n"))
            }
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn orchestrator() -> AgentProfile {
        AgentProfile::new("orch-1", "Orchestrator", AgentTier::Orchestrator)
    }

    fn system_agent(id: &str) -> AgentProfile {
        AgentProfile::new(id, &format!("System-{id}"), AgentTier::System)
    }

    fn user_agent(id: &str) -> AgentProfile {
        AgentProfile::new(id, &format!("User-{id}"), AgentTier::User)
    }

    // ── AgentTier ───

    #[test]
    fn tier_rank_ordering() {
        assert_eq!(AgentTier::Orchestrator.rank(), 0);
        assert_eq!(AgentTier::System.rank(), 1);
        assert_eq!(AgentTier::User.rank(), 2);
    }

    #[test]
    fn tier_delegation_rules() {
        assert!(AgentTier::Orchestrator.can_delegate_to(AgentTier::System));
        assert!(AgentTier::Orchestrator.can_delegate_to(AgentTier::User));
        assert!(AgentTier::System.can_delegate_to(AgentTier::User));
        assert!(!AgentTier::User.can_delegate_to(AgentTier::System));
        assert!(!AgentTier::User.can_delegate_to(AgentTier::Orchestrator));
        assert!(!AgentTier::System.can_delegate_to(AgentTier::Orchestrator));
        // Same tier cannot delegate to itself
        assert!(!AgentTier::System.can_delegate_to(AgentTier::System));
    }

    #[test]
    fn tier_as_str_and_from_str() {
        assert_eq!(AgentTier::Orchestrator.as_str(), "orchestrator");
        assert_eq!(AgentTier::from_str_lossy("system"), AgentTier::System);
        assert_eq!(
            AgentTier::from_str_lossy("ORCHESTRATOR"),
            AgentTier::Orchestrator
        );
        assert_eq!(AgentTier::from_str_lossy("unknown"), AgentTier::User);
    }

    // ── AgentProfile ───

    #[test]
    fn profile_defaults_by_tier() {
        let orch = orchestrator();
        assert!(orch.can_delegate);
        assert_eq!(orch.max_delegation_depth, 3);

        let sys = system_agent("s1");
        assert!(sys.can_delegate);
        assert_eq!(sys.max_delegation_depth, 1);

        let usr = user_agent("u1");
        assert!(!usr.can_delegate);
        assert_eq!(usr.max_delegation_depth, 0);
    }

    #[test]
    fn delegation_permission_check() {
        let orch = orchestrator();
        let sys = system_agent("s1");
        let usr = user_agent("u1");

        assert!(orch.can_delegate_to_agent(&sys));
        assert!(orch.can_delegate_to_agent(&usr));
        assert!(sys.can_delegate_to_agent(&usr));
        assert!(!usr.can_delegate_to_agent(&sys));
        assert!(!usr.can_delegate_to_agent(&orch));
    }

    #[test]
    fn delegation_restricted_by_delegate_to_list() {
        let mut orch = orchestrator();
        orch.delegate_to = vec!["s1".to_string()];

        let s1 = system_agent("s1");
        let s2 = system_agent("s2");

        assert!(orch.can_delegate_to_agent(&s1));
        assert!(!orch.can_delegate_to_agent(&s2));
    }

    #[test]
    fn user_agent_cannot_delegate_even_with_flag() {
        let mut usr = user_agent("u1");
        usr.can_delegate = true; // override flag
        let usr2 = user_agent("u2");
        // Still can't: same tier
        assert!(!usr.can_delegate_to_agent(&usr2));
    }

    // ── AgentProfileRegistry ───

    #[test]
    fn registry_register_and_get() {
        let mut reg = AgentProfileRegistry::new();
        reg.register(orchestrator()).unwrap();
        reg.register(system_agent("s1")).unwrap();

        assert!(reg.get("orch-1").is_some());
        assert!(reg.get("s1").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn registry_list_by_tier() {
        let mut reg = AgentProfileRegistry::new();
        reg.register(orchestrator()).unwrap();
        reg.register(system_agent("s1")).unwrap();
        reg.register(system_agent("s2")).unwrap();
        reg.register(user_agent("u1")).unwrap();

        assert_eq!(reg.list_by_tier(AgentTier::Orchestrator).len(), 1);
        assert_eq!(reg.list_by_tier(AgentTier::System).len(), 2);
        assert_eq!(reg.list_by_tier(AgentTier::User).len(), 1);
    }

    #[test]
    fn registry_find_delegates() {
        let mut reg = AgentProfileRegistry::new();
        let orch = orchestrator();
        reg.register(orch.clone()).unwrap();
        reg.register(system_agent("s1")).unwrap();
        reg.register(system_agent("s2")).unwrap();
        reg.register(user_agent("u1")).unwrap();

        let delegates = reg.find_delegates(&orch);
        assert_eq!(delegates.len(), 3); // s1, s2, u1

        let sys = system_agent("s1");
        let delegates = reg.find_delegates(&sys);
        assert_eq!(delegates.len(), 1); // u1 only
    }

    #[test]
    fn registry_remove() {
        let mut reg = AgentProfileRegistry::new();
        reg.register(system_agent("s1")).unwrap();
        assert!(reg.get("s1").is_some());
        reg.remove("s1");
        assert!(reg.get("s1").is_none());
    }

    // ── Delegation Validation ───

    #[test]
    fn validate_delegation_success() {
        let mut reg = AgentProfileRegistry::new();
        reg.register(orchestrator()).unwrap();
        reg.register(system_agent("s1")).unwrap();
        reg.register(system_agent("s2")).unwrap();

        let req = DelegationRequest {
            delegation_id: "d1".into(),
            parent_run_id: "run-1".into(),
            task: "analyze code".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["s1".into(), "s2".into()],
                aggregation: AggregationStrategy::FirstSuccess,
                timeout_sec: 300,
            },
            user_id: "u1".into(),
            depth: 0,
            context: HashMap::new(),
        };

        assert!(reg.validate_delegation(&req, "orch-1").is_ok());
    }

    #[test]
    fn validate_delegation_depth_exceeded() {
        let mut reg = AgentProfileRegistry::new();
        reg.register(orchestrator()).unwrap();
        reg.register(system_agent("s1")).unwrap();

        let req = DelegationRequest {
            delegation_id: "d1".into(),
            parent_run_id: "run-1".into(),
            task: "deep".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["s1".into()],
                stop_on_success: true,
            },
            user_id: "u1".into(),
            depth: 5, // exceeds max_delegation_depth=3
            context: HashMap::new(),
        };

        let err = reg.validate_delegation(&req, "orch-1").unwrap_err();
        assert!(err.contains("depth"));
    }

    #[test]
    fn validate_delegation_wrong_tier() {
        let mut reg = AgentProfileRegistry::new();
        reg.register(system_agent("s1")).unwrap();
        reg.register(orchestrator()).unwrap();

        let req = DelegationRequest {
            delegation_id: "d1".into(),
            parent_run_id: "run-1".into(),
            task: "upward".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["orch-1".into()],
                stop_on_success: true,
            },
            user_id: "u1".into(),
            depth: 0,
            context: HashMap::new(),
        };

        let err = reg.validate_delegation(&req, "s1").unwrap_err();
        assert!(err.contains("cannot delegate"));
    }

    #[test]
    fn validate_delegation_user_cannot_delegate() {
        let mut reg = AgentProfileRegistry::new();
        reg.register(user_agent("u1")).unwrap();
        reg.register(user_agent("u2")).unwrap();

        let req = DelegationRequest {
            delegation_id: "d1".into(),
            parent_run_id: "run-1".into(),
            task: "help".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["u2".into()],
                stop_on_success: true,
            },
            user_id: "u1".into(),
            depth: 0,
            context: HashMap::new(),
        };

        let err = reg.validate_delegation(&req, "u1").unwrap_err();
        assert!(err.contains("cannot delegate"));
    }

    #[test]
    fn validate_delegation_unknown_source() {
        let reg = AgentProfileRegistry::new();
        let req = DelegationRequest {
            delegation_id: "d1".into(),
            parent_run_id: "run-1".into(),
            task: "test".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec![],
                stop_on_success: true,
            },
            user_id: "u1".into(),
            depth: 0,
            context: HashMap::new(),
        };

        let err = reg.validate_delegation(&req, "unknown").unwrap_err();
        assert!(err.contains("not registered"));
    }

    #[test]
    fn validate_delegation_unknown_target() {
        let mut reg = AgentProfileRegistry::new();
        reg.register(orchestrator()).unwrap();

        let req = DelegationRequest {
            delegation_id: "d1".into(),
            parent_run_id: "run-1".into(),
            task: "test".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["nonexistent".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 60,
            },
            user_id: "u1".into(),
            depth: 0,
            context: HashMap::new(),
        };

        let err = reg.validate_delegation(&req, "orch-1").unwrap_err();
        assert!(err.contains("not registered"));
    }

    // ── Coordination Pattern Validation ───

    #[test]
    fn validate_pipeline_pattern() {
        let mut reg = AgentProfileRegistry::new();
        reg.register(orchestrator()).unwrap();
        reg.register(system_agent("coder")).unwrap();
        reg.register(system_agent("reviewer")).unwrap();

        let req = DelegationRequest {
            delegation_id: "d1".into(),
            parent_run_id: "run-1".into(),
            task: "write and review".into(),
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
            user_id: "u1".into(),
            depth: 0,
            context: HashMap::new(),
        };

        assert!(reg.validate_delegation(&req, "orch-1").is_ok());
    }

    #[test]
    fn validate_adversarial_review_pattern() {
        let mut reg = AgentProfileRegistry::new();
        reg.register(orchestrator()).unwrap();
        reg.register(system_agent("writer")).unwrap();
        reg.register(system_agent("critic")).unwrap();

        let req = DelegationRequest {
            delegation_id: "d1".into(),
            parent_run_id: "run-1".into(),
            task: "write with review".into(),
            pattern: CoordinationPattern::AdversarialReview {
                producer_id: "writer".into(),
                reviewer_id: "critic".into(),
                max_rounds: 3,
                acceptance_threshold: 0.8,
            },
            user_id: "u1".into(),
            depth: 0,
            context: HashMap::new(),
        };

        assert!(reg.validate_delegation(&req, "orch-1").is_ok());
    }

    // ── Result Aggregation ───

    fn make_result(agent_id: &str, output: &str) -> AgentResult {
        AgentResult {
            agent_id: agent_id.into(),
            run_id: format!("run-{agent_id}"),
            status: "completed".into(),
            output: Some(output.into()),
            error: None,
            prompt_tokens: 100,
            completion_tokens: 50,
            tool_calls: 3,
        }
    }

    fn make_failed(agent_id: &str) -> AgentResult {
        AgentResult {
            agent_id: agent_id.into(),
            run_id: format!("run-{agent_id}"),
            status: "failed".into(),
            output: None,
            error: Some("timeout".into()),
            prompt_tokens: 50,
            completion_tokens: 0,
            tool_calls: 0,
        }
    }

    #[test]
    fn aggregate_first_success() {
        let results = vec![
            make_failed("a1"),
            make_result("a2", "answer"),
            make_result("a3", "other"),
        ];
        let out = aggregate_results(&AggregationStrategy::FirstSuccess, &results);
        assert_eq!(out.as_deref(), Some("answer"));
    }

    #[test]
    fn aggregate_first_success_all_failed() {
        let results = vec![make_failed("a1"), make_failed("a2")];
        assert!(aggregate_results(&AggregationStrategy::FirstSuccess, &results).is_none());
    }

    #[test]
    fn aggregate_all_results() {
        let results = vec![
            make_result("a1", "one"),
            make_failed("a2"),
            make_result("a3", "three"),
        ];
        let out = aggregate_results(&AggregationStrategy::AllResults, &results).unwrap();
        assert!(out.contains("one"));
        assert!(out.contains("three"));
        assert!(out.contains("---"));
    }

    #[test]
    fn aggregate_consensus() {
        let results = vec![
            make_result("a1", "yes"),
            make_result("a2", "no"),
            make_result("a3", "yes"),
        ];
        let out = aggregate_results(&AggregationStrategy::Consensus, &results);
        assert_eq!(out.as_deref(), Some("yes"));
    }

    #[test]
    fn aggregate_llm_guided_formats_outputs() {
        let results = vec![make_result("a1", "output1"), make_result("a2", "output2")];
        let out = aggregate_results(
            &AggregationStrategy::LlmGuided {
                prompt_template: "merge: {outputs}".into(),
            },
            &results,
        )
        .unwrap();
        assert!(out.contains("Agent 1 (a1)"));
        assert!(out.contains("Agent 2 (a2)"));
        assert!(out.contains("output1"));
        assert!(out.contains("output2"));
    }

    // ── DelegationResult ───

    #[test]
    fn delegation_result_from_results_all_success() {
        let results = vec![make_result("a1", "one"), make_result("a2", "two")];
        let dr = DelegationResult::from_results("d1", results, Some("merged".into()));
        assert_eq!(dr.status, "completed");
        assert_eq!(dr.total_prompt_tokens, 200);
        assert_eq!(dr.total_completion_tokens, 100);
        assert_eq!(dr.total_tool_calls, 6);
        assert_eq!(dr.aggregated_output.as_deref(), Some("merged"));
    }

    #[test]
    fn delegation_result_partial_success() {
        let results = vec![make_result("a1", "one"), make_failed("a2")];
        let dr = DelegationResult::from_results("d1", results, None);
        assert_eq!(dr.status, "partial");
    }

    #[test]
    fn delegation_result_all_failed() {
        let results = vec![make_failed("a1"), make_failed("a2")];
        let dr = DelegationResult::from_results("d1", results, None);
        assert_eq!(dr.status, "failed");
        assert_eq!(dr.total_prompt_tokens, 100);
    }

    #[test]
    fn agent_result_is_success() {
        assert!(make_result("a1", "ok").is_success());
        assert!(!make_failed("a1").is_success());
    }

    // ── Serialization ───

    #[test]
    fn coordination_pattern_serializes_as_tagged() {
        let pattern = CoordinationPattern::FanOut {
            agent_ids: vec!["a1".into()],
            aggregation: AggregationStrategy::FirstSuccess,
            timeout_sec: 60,
        };
        let json = serde_json::to_value(&pattern).unwrap();
        assert_eq!(json["pattern"], "fan_out");
        assert_eq!(json["timeout_sec"], 60);
    }

    #[test]
    fn agent_profile_round_trip_json() {
        let profile = orchestrator();
        let json = serde_json::to_string(&profile).unwrap();
        let restored: AgentProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.agent_id, "orch-1");
        assert_eq!(restored.tier, AgentTier::Orchestrator);
        assert!(restored.can_delegate);
    }

    #[test]
    fn delegation_request_round_trip() {
        let req = DelegationRequest {
            delegation_id: "d1".into(),
            parent_run_id: "run-1".into(),
            task: "test".into(),
            pattern: CoordinationPattern::AdversarialReview {
                producer_id: "w".into(),
                reviewer_id: "r".into(),
                max_rounds: 3,
                acceptance_threshold: 0.8,
            },
            user_id: "u1".into(),
            depth: 1,
            context: HashMap::new(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let restored: DelegationRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.delegation_id, "d1");
        assert_eq!(restored.depth, 1);
    }
}
