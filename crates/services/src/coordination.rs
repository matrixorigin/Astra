//! Multi-agent coordination framework.
//!
//! Provides agent profiles with explicit delegation permissions, coordination patterns
//! for multi-agent task execution, delegation engine for spawning sub-runs,
//! and result aggregation strategies.
//!
//! Tiers label profiles and provide construction defaults. Runtime delegation
//! uses the source's explicit permission, target scope, depth and ancestor
//! chain; equal-tier agents may collaborate without granting new capabilities.
//!
//! # Coordination Patterns
//!
//! - **FanOut**: Dispatch task to N agents in parallel, aggregate results
//! - **Sequential**: Ordered execution with output handoff and optional early exit

use astra_core::SubRunState;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const AGENT_RESULT_STATUS_COMPLETED: &str = "completed";
pub const AGENT_RESULT_STATUS_DELEGATED: &str = "delegated";
pub const AGENT_RESULT_STATUS_FAILED: &str = "failed";
pub const AGENT_RESULT_STATUS_TIMEOUT: &str = "timeout";
pub const AGENT_RESULT_STATUS_WAITING: &str = "waiting";
pub const AGENT_RESULT_STATUS_PAUSED: &str = "paused";
pub const AGENT_RESULT_STATUS_CANCELLED: &str = "cancelled";
pub const AGENT_RESULT_STATUS_PARTIAL: &str = "partial";
pub const AGENT_RESULT_STATUS_VERIFICATION_FAILED: &str = "verification_failed";
/// Typed durable error code used when a terminal failed run represents an
/// interrupted child with a usable partial result.
pub const AGENT_RESULT_PARTIAL_DURABLE_ERROR_CODE: &str = "agent_result_partial";
pub fn durable_agent_result_is_partial(error_code: Option<&str>) -> bool {
    error_code == Some(AGENT_RESULT_PARTIAL_DURABLE_ERROR_CODE)
}

pub fn durable_agent_partial_reason<'a>(
    error_code: Option<&str>,
    error_message: Option<&'a str>,
) -> Option<&'a str> {
    durable_agent_result_is_partial(error_code)
        .then_some(error_message)
        .flatten()
}

pub const DELEGATION_RESULT_STATUS_COMPLETED: &str = "completed";
pub const DELEGATION_RESULT_STATUS_UNFINISHED: &str = "unfinished";
pub const DELEGATION_RESULT_STATUS_PARTIAL: &str = "partial";
pub const DELEGATION_RESULT_STATUS_FAILED: &str = "failed";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentResultStatusKind {
    Completed,
    Delegated,
    Failed,
    Timeout,
    Waiting,
    Paused,
    Cancelled,
    Partial,
    VerificationFailed,
    Other,
}

pub fn agent_result_status_kind(status: &str) -> AgentResultStatusKind {
    let normalized = status.trim().to_ascii_lowercase();
    match normalized.as_str() {
        AGENT_RESULT_STATUS_COMPLETED => AgentResultStatusKind::Completed,
        AGENT_RESULT_STATUS_DELEGATED => AgentResultStatusKind::Delegated,
        AGENT_RESULT_STATUS_FAILED => AgentResultStatusKind::Failed,
        AGENT_RESULT_STATUS_TIMEOUT => AgentResultStatusKind::Timeout,
        AGENT_RESULT_STATUS_WAITING => AgentResultStatusKind::Waiting,
        AGENT_RESULT_STATUS_PAUSED => AgentResultStatusKind::Paused,
        AGENT_RESULT_STATUS_CANCELLED => AgentResultStatusKind::Cancelled,
        AGENT_RESULT_STATUS_PARTIAL => AgentResultStatusKind::Partial,
        AGENT_RESULT_STATUS_VERIFICATION_FAILED => AgentResultStatusKind::VerificationFailed,
        _ => AgentResultStatusKind::Other,
    }
}

pub fn agent_result_status_is_success(status: &str) -> bool {
    matches!(
        agent_result_status_kind(status),
        AgentResultStatusKind::Completed | AgentResultStatusKind::Delegated
    )
}

pub fn agent_result_status_is_unfinished(status: &str) -> bool {
    matches!(
        agent_result_status_kind(status),
        AgentResultStatusKind::Waiting | AgentResultStatusKind::Paused
    )
}

pub fn agent_result_status_is_failure(status: &str) -> bool {
    matches!(
        agent_result_status_kind(status),
        AgentResultStatusKind::Failed
            | AgentResultStatusKind::Timeout
            | AgentResultStatusKind::Cancelled
            | AgentResultStatusKind::VerificationFailed
    )
}

pub fn agent_result_status_to_subrun_state(status: &str) -> SubRunState {
    match agent_result_status_kind(status) {
        AgentResultStatusKind::Completed | AgentResultStatusKind::Delegated => {
            SubRunState::Completed
        }
        AgentResultStatusKind::Waiting => SubRunState::Waiting,
        AgentResultStatusKind::Paused => SubRunState::Paused,
        AgentResultStatusKind::Cancelled => SubRunState::Cancelled,
        AgentResultStatusKind::VerificationFailed => SubRunState::VerificationFailed,
        AgentResultStatusKind::Failed
        | AgentResultStatusKind::Timeout
        | AgentResultStatusKind::Partial
        | AgentResultStatusKind::Other => SubRunState::Failed,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationResultStatusKind {
    Completed,
    Unfinished,
    Partial,
    Failed,
    Other,
}

pub fn delegation_result_status_kind(status: &str) -> DelegationResultStatusKind {
    let normalized = status.trim().to_ascii_lowercase();
    match normalized.as_str() {
        DELEGATION_RESULT_STATUS_COMPLETED => DelegationResultStatusKind::Completed,
        DELEGATION_RESULT_STATUS_UNFINISHED => DelegationResultStatusKind::Unfinished,
        DELEGATION_RESULT_STATUS_PARTIAL => DelegationResultStatusKind::Partial,
        DELEGATION_RESULT_STATUS_FAILED => DelegationResultStatusKind::Failed,
        _ => DelegationResultStatusKind::Other,
    }
}

pub fn delegation_result_status_is_success(status: &str) -> bool {
    delegation_result_status_kind(status) == DelegationResultStatusKind::Completed
}

pub fn delegation_result_status_is_unfinished(status: &str) -> bool {
    delegation_result_status_kind(status) == DelegationResultStatusKind::Unfinished
}

// ─── Agent Profile ──────────────────────────────────────────────────────────

/// Agent classification used for display and profile-construction defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTier {
    /// Top-level coordinator; defaults to delegation enabled.
    Orchestrator,
    /// Specialist agent; defaults to delegation enabled.
    System,
    /// User-facing agent; defaults to delegation disabled.
    User,
}

impl AgentTier {
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProfile {
    /// Unique agent identifier.
    pub agent_id: String,
    /// Human-readable name.
    pub name: String,
    /// Classification; explicit controls below determine delegation authority.
    pub tier: AgentTier,
    /// Optional system prompt override for this agent.
    pub system_prompt: Option<String>,
    /// Filter limiting which skills this agent can use.
    /// Empty = unrestricted. Tool authorization is carried separately by
    /// `allow_tools`.
    pub skill_filter: Vec<String>,
    /// Optional execution-tool allowlist.
    ///
    /// `None` inherits the admitted tool surface; `Some(empty)` denies every
    /// tool governed by this allowlist. This is deliberately separate from
    /// `skill_filter`, which remains the skill-selection surface.
    #[serde(default)]
    pub allow_tools: Option<Vec<String>>,
    /// Whether this profile is restricted to read-only execution.
    #[serde(default)]
    pub read_only: bool,
    /// Optional initial adaptive turn slice.
    #[serde(default)]
    pub initial_turns: Option<u32>,
    /// Optional hard maximum turn budget.
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// Optional product-level Offering selected for this agent.
    /// Absence means the child inherits its parent's admitted Offering.
    pub model_selection: Option<astra_turn_types::ModelSelection>,
    /// Whether this agent can delegate tasks to sub-agents.
    pub can_delegate: bool,
    /// Explicit list of agent IDs this agent may delegate to.
    /// Empty = any other agent in the admitted registry. This never grants
    /// authority over a profile outside that registry.
    pub delegate_to: Vec<String>,
    /// Maximum delegation depth (prevents infinite loops).
    pub max_delegation_depth: u32,
    /// Additional metadata.
    pub metadata: HashMap<String, serde_json::Value>,
    /// MCP server names to connect when this agent starts (D-10).
    /// These are looked up from a project-level MCP config.
    pub mcp_servers: Vec<String>,
}

impl AgentProfile {
    pub fn new(agent_id: &str, name: &str, tier: AgentTier) -> Self {
        Self {
            agent_id: agent_id.to_string(),
            name: name.to_string(),
            tier,
            system_prompt: None,
            skill_filter: Vec::new(),
            allow_tools: None,
            read_only: false,
            initial_turns: None,
            max_turns: None,
            model_selection: None,
            can_delegate: tier != AgentTier::User,
            delegate_to: Vec::new(),
            max_delegation_depth: match tier {
                AgentTier::Orchestrator => 3,
                AgentTier::System => 1,
                AgentTier::User => 0,
            },
            metadata: HashMap::new(),
            mcp_servers: Vec::new(),
        }
    }

    /// Validate profile controls that must be safe before runtime admission.
    ///
    /// The runtime remains the final authority for effective permissions; this
    /// check only rejects malformed profile bounds before they become part of
    /// a registry or Team execution snapshot.
    pub fn validate_capability_bounds(&self) -> Result<(), String> {
        if self.agent_id.trim().is_empty() {
            return Err("agent profile agent_id must not be empty".to_string());
        }
        if self.initial_turns == Some(0) {
            return Err(format!(
                "agent profile '{}' initial_turns must be positive",
                self.agent_id
            ));
        }
        if self.max_turns == Some(0) {
            return Err(format!(
                "agent profile '{}' max_turns must be positive",
                self.agent_id
            ));
        }
        if let (Some(initial), Some(maximum)) = (self.initial_turns, self.max_turns) {
            if initial > maximum {
                return Err(format!(
                    "agent profile '{}' initial_turns cannot exceed max_turns",
                    self.agent_id
                ));
            }
        }
        Ok(())
    }

    /// Check if this agent can delegate to a specific target agent.
    pub fn can_delegate_to_agent(&self, target: &AgentProfile) -> bool {
        if !self.can_delegate || self.agent_id == target.agent_id {
            return false;
        }
        if !self.delegate_to.is_empty() && !self.delegate_to.contains(&target.agent_id) {
            return false;
        }
        true
    }
}

// ─── Coordination Patterns ──────────────────────────────────────────────────

/// Pattern for coordinating multiple agents on a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "pattern", rename_all = "snake_case", deny_unknown_fields)]
pub enum CoordinationPattern {
    /// Dispatch to N agents in parallel, aggregate results.
    FanOut {
        /// Agent IDs to dispatch to.
        agent_ids: Vec<String>,
        /// How to aggregate results.
        aggregation: AggregationStrategy,
        /// Maximum time per agent (seconds). 0 = no per-agent timeout.
        timeout_sec: u64,
    },

    /// Simple sequential delegation.
    Sequential {
        /// Ordered agent IDs.
        agent_ids: Vec<String>,
        /// Stop on first success?
        stop_on_success: bool,
        /// Maximum time per agent (seconds). 0 = no per-agent timeout.
        #[serde(default)]
        timeout_sec: u64,
    },

    /// Fork: dispatch N tasks sharing the parent's full conversation context.
    /// All fork children receive the same message prefix (enabling prompt cache sharing).
    /// Fork children cannot recursively fork or delegate.
    Fork {
        /// Per-child task descriptions.
        tasks: Vec<String>,
        /// Admitted agent profile to use for all fork children.
        agent_id: String,
        /// How to aggregate fork results.
        aggregation: AggregationStrategy,
        /// Maximum time per fork child (seconds). 0 = no per-child timeout.
        #[serde(default)]
        timeout_sec: u64,
    },
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
}

// ─── Delegation Request / Result ────────────────────────────────────────────

/// Request to delegate a task to one or more agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationRequest {
    /// Unique delegation ID.
    pub delegation_id: String,
    /// Canonical parent session identity. This scopes child run persistence,
    /// transcript recovery, and runtime tools; it is not task prompt context.
    pub session_id: String,
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
    /// Chain of agent_ids that led to this delegation (for circular detection).
    /// Format: ["orchestrator", "coder", "reviewer"] means orchestrator→coder→reviewer.
    #[serde(default)]
    pub delegation_chain: Vec<String>,
    /// Context to pass to delegated agents.
    pub context: HashMap<String, serde_json::Value>,
    /// UI/runtime execution binding metadata inherited from the parent run.
    /// Propagated to every child `SubRunConfig` so delegated agents resolve
    /// workspace/executor/transport bindings correctly.
    pub execution_metadata: Option<serde_json::Value>,
}

/// Result from a single agent's execution within a delegation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResult {
    /// Agent that produced this result.
    pub agent_id: String,
    /// Run ID for this agent's execution.
    pub run_id: String,
    /// Status taxonomy for a delegated sub-run result:
    /// `completed`, `delegated`, `failed`, `timeout`, `waiting`, `paused`,
    /// `cancelled`, `partial`, or `verification_failed`.
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
        agent_result_status_is_success(&self.status) && self.error.is_none()
    }

    pub fn is_unfinished(&self) -> bool {
        agent_result_status_is_unfinished(&self.status)
    }

    pub fn is_failure(&self) -> bool {
        agent_result_status_is_failure(&self.status) || self.error.is_some()
    }
}

/// Aggregated result from a coordination pattern.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationResult {
    /// Delegation ID.
    pub delegation_id: String,
    /// Overall status: `completed`, `unfinished`, `partial`, or `failed`.
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
    pub fn is_success(&self) -> bool {
        delegation_result_status_is_success(&self.status)
    }

    pub fn is_unfinished(&self) -> bool {
        delegation_result_status_is_unfinished(&self.status)
    }

    /// Aggregate token counts from agent results.
    pub fn from_results(
        delegation_id: &str,
        results: Vec<AgentResult>,
        aggregated_output: Option<String>,
    ) -> Self {
        let total_prompt: u64 = results.iter().map(|r| r.prompt_tokens).sum();
        let total_completion: u64 = results.iter().map(|r| r.completion_tokens).sum();
        let total_tools: u32 = results.iter().map(|r| r.tool_calls).sum();
        let has_results = !results.is_empty();
        let all_ok = has_results && results.iter().all(|r| r.is_success());
        let any_unfinished = results.iter().any(|r| r.is_unfinished());
        let any_ok = results.iter().any(|r| r.is_success());

        Self {
            delegation_id: delegation_id.to_string(),
            // Execution settlement and result synthesis are deliberately
            // separate. A successful agent is allowed to produce no text;
            // absence of an aggregate must not rewrite completed work as a
            // partial execution failure. Strategy-specific synthesis status
            // should be surfaced separately by the caller.
            status: if all_ok {
                DELEGATION_RESULT_STATUS_COMPLETED.to_string()
            } else if any_unfinished {
                DELEGATION_RESULT_STATUS_UNFINISHED.to_string()
            } else if any_ok {
                DELEGATION_RESULT_STATUS_PARTIAL.to_string()
            } else {
                DELEGATION_RESULT_STATUS_FAILED.to_string()
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

    /// Register one immutable profile identity.
    ///
    /// A registry is execution authority, not a last-writer-wins cache. Silent
    /// replacement lets an unrelated request change the profile observed by
    /// an already-admitted delegation.
    pub fn register(&mut self, profile: AgentProfile) -> Result<(), String> {
        profile.validate_capability_bounds()?;
        if self.profiles.contains_key(&profile.agent_id) {
            return Err(format!(
                "agent profile '{}' is already registered",
                profile.agent_id
            ));
        }
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
            CoordinationPattern::Sequential { agent_ids, .. } => agent_ids.clone(),
            CoordinationPattern::Fork { agent_id, .. } => vec![agent_id.clone()],
        };

        // Circular delegation detection: reject if any target agent already
        // appears in the delegation chain. This prevents A→B→C→A cycles
        // that the depth limit alone cannot catch.
        for target_id in &agent_ids {
            if request.delegation_chain.contains(target_id) {
                let chain_display = request
                    .delegation_chain
                    .iter()
                    .chain(std::iter::once(target_id))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" → ");
                return Err(format!(
                    "circular delegation detected: {chain_display}. \
                     Agent '{}' already exists in the delegation chain",
                    target_id
                ));
            }
        }

        if agent_ids.is_empty() {
            return Err("delegation pattern has no agents".to_string());
        }

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
            .filter(|result| result.is_success())
            .find_map(|result| {
                result
                    .output
                    .as_deref()
                    .filter(|output| !output.trim().is_empty())
                    .map(str::to_string)
            }),

        AggregationStrategy::AllResults => {
            let outputs: Vec<String> = results
                .iter()
                .filter(|r| r.is_success())
                .filter_map(|r| r.output.clone())
                .filter(|output| !output.trim().is_empty())
                .collect();
            if outputs.is_empty() {
                None
            } else {
                // Cap aggregated output to prevent unbounded concatenation.
                // 64KB allows ~1000 typical agent responses while preventing
                // memory exhaustion in pathological cases.
                const MAX_AGGREGATED_BYTES: usize = 65_536;
                let mut joined = String::new();
                for (i, output) in outputs.iter().enumerate() {
                    let separator = if i == 0 { "" } else { "\n---\n" };
                    if joined.len() + separator.len() + output.len() > MAX_AGGREGATED_BYTES {
                        let marker = format!(
                            "\n... ({} more results truncated, {} total exceeded {} byte cap)",
                            outputs.len() - i,
                            outputs.len(),
                            MAX_AGGREGATED_BYTES
                        );
                        let content_limit = MAX_AGGREGATED_BYTES.saturating_sub(marker.len());
                        while joined.len() > content_limit {
                            joined.pop();
                        }
                        joined.push_str(&marker);
                        debug_assert!(joined.len() <= MAX_AGGREGATED_BYTES);
                        break;
                    }
                    joined.push_str(separator);
                    joined.push_str(output);
                }
                Some(joined)
            }
        }

        AggregationStrategy::Consensus => {
            // Consensus is a strict-majority contract, not an arbitrary
            // plurality. Normalize only transport-insignificant whitespace;
            // semantic synthesis belongs to the parent agent and must not be
            // guessed with fuzzy string similarity.
            let outputs: Vec<&str> = results
                .iter()
                .filter(|result| result.is_success())
                .filter_map(|result| result.output.as_deref())
                .filter(|output| !output.trim().is_empty())
                .collect();
            let mut counts: HashMap<String, (usize, &str)> = HashMap::new();
            for output in &outputs {
                let normalized = output
                    .lines()
                    .map(str::trim_end)
                    .collect::<Vec<_>>()
                    .join("\n")
                    .trim()
                    .to_string();
                let entry = counts.entry(normalized).or_insert((0, output));
                entry.0 += 1;
            }
            counts
                .into_values()
                .filter(|(count, _)| count.saturating_mul(2) > outputs.len())
                .max_by_key(|(count, _)| *count)
                .map(|(_, output)| output.trim().to_string())
        }
    }
}

// ─── Coordination Pattern Auto-Selection ────────────────────────────────────

/// Hints for automatic coordination pattern selection.
#[derive(Debug, Clone, Default)]
pub struct CoordinationHints {
    /// Agent IDs available for this delegation.
    pub agent_ids: Vec<String>,
    /// Whether sub-tasks have ordering dependencies.
    pub has_dependencies: bool,
    /// Default timeout per agent (seconds). 0 = no timeout.
    pub timeout_sec: u64,
}

/// Suggest a coordination pattern from typed orchestration facts.
///
/// Rules (in priority order):
/// 1. `has_dependencies` → Sequential (ordered)
/// 2. 2+ independent agents → FanOut
/// 3. Fallback → Sequential { stop_on_success: true }
///
/// Task prose is deliberately not accepted here. Natural-language keyword
/// matching is not a reliable semantic classifier and must never silently
/// change execution topology. Callers that need a `Fork` must provide that
/// pattern and its typed task list explicitly.
pub fn suggest_pattern(hints: &CoordinationHints) -> CoordinationPattern {
    let n = hints.agent_ids.len();
    let timeout = hints.timeout_sec;

    // No agents → Sequential with stop_on_success (nothing to run)
    if n == 0 {
        return CoordinationPattern::Sequential {
            agent_ids: vec![],
            stop_on_success: true,
            timeout_sec: timeout,
        };
    }

    // Rule 2: Dependency chain → Sequential
    if hints.has_dependencies && n >= 2 {
        return CoordinationPattern::Sequential {
            agent_ids: hints.agent_ids.clone(),
            stop_on_success: false,
            timeout_sec: timeout,
        };
    }

    // Rule 3: Multiple independent agents → FanOut
    if n >= 2 {
        return CoordinationPattern::FanOut {
            agent_ids: hints.agent_ids.clone(),
            aggregation: AggregationStrategy::AllResults,
            timeout_sec: timeout,
        };
    }

    // Fallback: Sequential
    CoordinationPattern::Sequential {
        agent_ids: hints.agent_ids.clone(),
        stop_on_success: true,
        timeout_sec: timeout,
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_profile_selects_an_offering_not_a_provider_model_name() {
        let mut profile = AgentProfile::new("reviewer", "Reviewer", AgentTier::User);
        profile.model_selection = Some(astra_turn_types::ModelSelection {
            offering_id: "offer-review".to_string(),
        });

        let encoded = serde_json::to_value(&profile).expect("serialize agent profile");
        assert_eq!(encoded["model_selection"]["offering_id"], "offer-review");
        assert!(encoded.get("model_override").is_none());

        let legacy = serde_json::json!({
            "agent_id": "reviewer",
            "name": "Reviewer",
            "tier": "user",
            "system_prompt": null,
            "skill_filter": [],
            "model_override": "provider-model",
            "can_delegate": false,
            "delegate_to": [],
            "max_delegation_depth": 0,
            "metadata": {},
            "mcp_servers": []
        });
        assert!(
            serde_json::from_value::<AgentProfile>(legacy).is_err(),
            "bare model names must not be silently accepted at an Offering boundary"
        );
    }

    fn orchestrator() -> AgentProfile {
        AgentProfile::new("orch-1", "Orchestrator", AgentTier::Orchestrator)
    }

    fn system_agent(id: &str) -> AgentProfile {
        AgentProfile::new(id, &format!("System-{id}"), AgentTier::System)
    }

    fn user_agent(id: &str) -> AgentProfile {
        AgentProfile::new(id, &format!("User-{id}"), AgentTier::User)
    }

    #[test]
    fn durable_partial_result_uses_typed_error_code() {
        let reason = "budget_exhausted: adaptive hard turn limit reached";
        assert!(durable_agent_result_is_partial(Some(
            AGENT_RESULT_PARTIAL_DURABLE_ERROR_CODE
        ),));
        assert_eq!(
            durable_agent_partial_reason(
                Some(AGENT_RESULT_PARTIAL_DURABLE_ERROR_CODE),
                Some(reason),
            ),
            Some(reason)
        );
        assert!(!durable_agent_result_is_partial(None));
        assert!(!durable_agent_result_is_partial(Some("ordinary_failure")));
        assert_eq!(
            durable_agent_partial_reason(None, Some("partial:budget_exhausted")),
            None
        );
    }

    // ── AgentTier ───

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
        assert!(orch.allow_tools.is_none());
        assert!(!orch.read_only);
        assert!(orch.initial_turns.is_none());
        assert!(orch.max_turns.is_none());

        let sys = system_agent("s1");
        assert!(sys.can_delegate);
        assert_eq!(sys.max_delegation_depth, 1);

        let usr = user_agent("u1");
        assert!(!usr.can_delegate);
        assert_eq!(usr.max_delegation_depth, 0);
    }

    #[test]
    fn profile_registry_preserves_explicit_tool_and_turn_controls() {
        let mut profile = user_agent("bounded");
        profile.allow_tools = Some(Vec::new());
        profile.read_only = true;
        profile.initial_turns = Some(2);
        profile.max_turns = Some(5);

        let mut registry = AgentProfileRegistry::new();
        registry.register(profile).unwrap();
        let retained = registry.get("bounded").unwrap();
        assert_eq!(retained.allow_tools, Some(Vec::new()));
        assert!(retained.read_only);
        assert_eq!(retained.initial_turns, Some(2));
        assert_eq!(retained.max_turns, Some(5));
    }

    #[test]
    fn profile_registry_rejects_zero_initial_turns() {
        let mut profile = user_agent("invalid");
        profile.initial_turns = Some(0);

        let mut registry = AgentProfileRegistry::new();
        let error = registry.register(profile).unwrap_err();
        assert!(error.contains("initial_turns must be positive"));
    }

    #[test]
    fn profile_registry_rejects_duplicate_identity_without_overwrite() {
        let mut registry = AgentProfileRegistry::new();
        let mut original = AgentProfile::new("coder", "builtin coder", AgentTier::System);
        original.system_prompt = Some("trusted".to_string());
        registry.register(original).unwrap();
        let mut replacement = AgentProfile::new("coder", "tenant coder", AgentTier::User);
        replacement.system_prompt = Some("polluted".to_string());

        assert!(registry.register(replacement).is_err());
        let retained = registry.get("coder").unwrap();
        assert_eq!(retained.name, "builtin coder");
        assert_eq!(retained.system_prompt.as_deref(), Some("trusted"));
    }

    #[test]
    fn delegation_permission_check() {
        let orch = orchestrator();
        let sys = system_agent("s1");
        let usr = user_agent("u1");

        assert!(orch.can_delegate_to_agent(&sys));
        assert!(orch.can_delegate_to_agent(&usr));
        assert!(sys.can_delegate_to_agent(&usr));
        assert!(sys.can_delegate_to_agent(&system_agent("s2")));
        assert!(sys.can_delegate_to_agent(&orch));
        assert!(!sys.can_delegate_to_agent(&sys));
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
    fn explicit_delegation_permission_is_not_overridden_by_tier() {
        let mut usr = user_agent("u1");
        usr.can_delegate = true; // override flag
        let usr2 = user_agent("u2");
        assert!(usr.can_delegate_to_agent(&usr2));
        usr.can_delegate = false;
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
            session_id: "test-session".into(),
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
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        assert!(reg.validate_delegation(&req, "orch-1").is_ok());
    }

    #[test]
    fn validate_delegation_depth_exceeded() {
        let mut reg = AgentProfileRegistry::new();
        reg.register(orchestrator()).unwrap();
        reg.register(system_agent("s1")).unwrap();

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "d1".into(),
            parent_run_id: "run-1".into(),
            task: "deep".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["s1".into()],
                stop_on_success: true,
                timeout_sec: 0,
            },
            user_id: "u1".into(),
            depth: 5, // exceeds max_delegation_depth=3
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let err = reg.validate_delegation(&req, "orch-1").unwrap_err();
        assert!(err.contains("depth"));
    }

    #[test]
    fn validate_delegation_honors_explicit_profile_depth_above_default() {
        let mut reg = AgentProfileRegistry::new();
        let mut source = orchestrator();
        source.max_delegation_depth = 6;
        reg.register(source).unwrap();
        reg.register(system_agent("s1")).unwrap();

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "d-deep".into(),
            parent_run_id: "run-deep".into(),
            task: "bounded recursive orchestration".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["s1".into()],
                stop_on_success: true,
                timeout_sec: 0,
            },
            user_id: "u1".into(),
            depth: 4,
            delegation_chain: vec!["root".into(), "planner".into()],
            context: HashMap::new(),
            execution_metadata: None,
        };

        assert!(reg.validate_delegation(&req, "orch-1").is_ok());
    }

    #[test]
    fn validate_delegation_uses_explicit_scope_depth_and_cycle_not_tier() {
        let mut reg = AgentProfileRegistry::new();
        let mut source = system_agent("s1");
        source.delegate_to = vec!["s2".into(), "orch-1".into(), "s1".into()];
        reg.register(source).unwrap();
        reg.register(system_agent("s2")).unwrap();
        reg.register(system_agent("outside-scope")).unwrap();
        reg.register(orchestrator()).unwrap();

        let mut req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "d1".into(),
            parent_run_id: "run-1".into(),
            task: "upward".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["orch-1".into()],
                stop_on_success: true,
                timeout_sec: 0,
            },
            user_id: "u1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        reg.validate_delegation(&req, "s1").unwrap();
        for (target, allowed) in [("s2", true), ("s1", false), ("outside-scope", false)] {
            req.pattern = CoordinationPattern::Fork {
                agent_id: target.into(),
                tasks: vec!["bounded task".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 0,
            };
            assert_eq!(reg.validate_delegation(&req, "s1").is_ok(), allowed);
        }
        req.pattern = CoordinationPattern::Sequential {
            agent_ids: vec!["s2".into()],
            stop_on_success: false,
            timeout_sec: 0,
        };
        req.depth = 1;
        assert!(
            reg.validate_delegation(&req, "s1")
                .unwrap_err()
                .contains("depth")
        );
        req.depth = 0;
        req.delegation_chain = vec!["s2".into()];
        assert!(
            reg.validate_delegation(&req, "s1")
                .unwrap_err()
                .contains("circular")
        );
    }

    #[test]
    fn validate_delegation_user_cannot_delegate() {
        let mut reg = AgentProfileRegistry::new();
        reg.register(user_agent("u1")).unwrap();
        reg.register(user_agent("u2")).unwrap();

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "d1".into(),
            parent_run_id: "run-1".into(),
            task: "help".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["u2".into()],
                stop_on_success: true,
                timeout_sec: 0,
            },
            user_id: "u1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let err = reg.validate_delegation(&req, "u1").unwrap_err();
        assert!(err.contains("cannot delegate"));
    }

    #[test]
    fn validate_delegation_unknown_source() {
        let reg = AgentProfileRegistry::new();
        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "d1".into(),
            parent_run_id: "run-1".into(),
            task: "test".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec![],
                stop_on_success: true,
                timeout_sec: 0,
            },
            user_id: "u1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let err = reg.validate_delegation(&req, "unknown").unwrap_err();
        assert!(err.contains("not registered"));
    }

    #[test]
    fn validate_delegation_unknown_target() {
        let mut reg = AgentProfileRegistry::new();
        reg.register(orchestrator()).unwrap();

        let req = DelegationRequest {
            session_id: "test-session".into(),
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
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let err = reg.validate_delegation(&req, "orch-1").unwrap_err();
        assert!(err.contains("not registered"));
    }

    // ── Coordination Pattern Validation ───

    #[test]
    fn validate_ordered_sequential_pattern() {
        let mut reg = AgentProfileRegistry::new();
        reg.register(orchestrator()).unwrap();
        reg.register(system_agent("coder")).unwrap();
        reg.register(system_agent("reviewer")).unwrap();

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "d1".into(),
            parent_run_id: "run-1".into(),
            task: "write and review".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into(), "reviewer".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "u1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        assert!(reg.validate_delegation(&req, "orch-1").is_ok());
    }

    fn make_result(agent_id: &str, output: &str) -> AgentResult {
        AgentResult {
            agent_id: agent_id.into(),
            run_id: format!("run-{agent_id}"),
            status: AGENT_RESULT_STATUS_COMPLETED.into(),
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
            status: AGENT_RESULT_STATUS_FAILED.into(),
            output: None,
            error: Some("timeout".into()),
            prompt_tokens: 50,
            completion_tokens: 0,
            tool_calls: 0,
        }
    }

    fn make_unfinished(agent_id: &str) -> AgentResult {
        AgentResult {
            agent_id: agent_id.into(),
            run_id: format!("run-{agent_id}"),
            status: AGENT_RESULT_STATUS_WAITING.into(),
            output: Some("waiting for approval".into()),
            error: None,
            prompt_tokens: 25,
            completion_tokens: 5,
            tool_calls: 1,
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
    fn aggregate_all_results_enforces_strict_byte_cap_without_invalid_utf8() {
        let results = vec![
            make_result("a1", &"界".repeat(30_000)),
            make_result("a2", &"second".repeat(20_000)),
        ];
        let out = aggregate_results(&AggregationStrategy::AllResults, &results).unwrap();
        assert!(out.len() <= 65_536);
        assert!(out.contains("results truncated"));
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn aggregation_ignores_blank_success_output() {
        let results = vec![make_result("a1", "  \n"), make_result("a2", "answer")];
        assert_eq!(
            aggregate_results(&AggregationStrategy::FirstSuccess, &results).as_deref(),
            Some("answer")
        );
        assert_eq!(
            aggregate_results(&AggregationStrategy::AllResults, &results).as_deref(),
            Some("answer")
        );
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
    fn aggregate_consensus_requires_a_strict_majority() {
        let results = vec![
            make_result("a1", "alpha"),
            make_result("a2", "beta"),
            make_result("a3", "gamma"),
        ];
        assert!(aggregate_results(&AggregationStrategy::Consensus, &results).is_none());
    }

    #[test]
    fn aggregate_consensus_ignores_transport_whitespace_only() {
        let results = vec![
            make_result("a1", "answer  \r\nsecond line\n"),
            make_result("a2", "answer\nsecond line"),
            make_result("a3", "different"),
        ];
        let out = aggregate_results(&AggregationStrategy::Consensus, &results);
        assert_eq!(out.as_deref(), Some("answer  \r\nsecond line"));
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
    fn successful_execution_without_synthesized_text_remains_completed() {
        let mut result = make_result("a1", "ignored");
        result.output = None;
        let delegation = DelegationResult::from_results("d1", vec![result], None);
        assert_eq!(delegation.status, DELEGATION_RESULT_STATUS_COMPLETED);
        assert!(delegation.aggregated_output.is_none());
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
    fn delegation_result_is_success_only_for_completed_status() {
        let completed = DelegationResult::from_results("d1", vec![make_result("a1", "one")], None);
        assert!(completed.is_success());

        let partial = DelegationResult::from_results(
            "d2",
            vec![make_result("a1", "one"), make_failed("a2")],
            None,
        );
        assert!(!partial.is_success());

        let failed = DelegationResult::from_results("d3", vec![make_failed("a1")], None);
        assert!(!failed.is_success());
    }

    #[test]
    fn agent_result_is_success() {
        assert!(make_result("a1", "ok").is_success());
        assert!(!make_failed("a1").is_success());
        assert!(!make_unfinished("a2").is_success());
    }

    #[test]
    fn agent_result_status_helpers_distinguish_success_failure_and_unfinished() {
        assert_eq!(
            agent_result_status_kind(AGENT_RESULT_STATUS_COMPLETED),
            AgentResultStatusKind::Completed
        );
        assert_eq!(
            agent_result_status_kind(AGENT_RESULT_STATUS_DELEGATED),
            AgentResultStatusKind::Delegated
        );
        assert_eq!(
            agent_result_status_kind(AGENT_RESULT_STATUS_VERIFICATION_FAILED),
            AgentResultStatusKind::VerificationFailed
        );
        assert_eq!(
            agent_result_status_kind(AGENT_RESULT_STATUS_WAITING),
            AgentResultStatusKind::Waiting
        );
        assert!(agent_result_status_is_success(
            AGENT_RESULT_STATUS_COMPLETED
        ));
        assert!(agent_result_status_is_success(
            AGENT_RESULT_STATUS_DELEGATED
        ));
        assert_eq!(
            agent_result_status_to_subrun_state(AGENT_RESULT_STATUS_DELEGATED),
            SubRunState::Completed
        );
        assert!(agent_result_status_is_failure(
            AGENT_RESULT_STATUS_VERIFICATION_FAILED
        ));
        assert!(agent_result_status_is_unfinished(
            AGENT_RESULT_STATUS_PAUSED
        ));
        assert!(!agent_result_status_is_unfinished(
            AGENT_RESULT_STATUS_DELEGATED
        ));
        assert!(!agent_result_status_is_failure(AGENT_RESULT_STATUS_PAUSED));
        assert!(make_unfinished("a2").is_unfinished());
        assert_eq!(
            agent_result_status_to_subrun_state(AGENT_RESULT_STATUS_WAITING),
            SubRunState::Waiting
        );
        assert_eq!(
            agent_result_status_to_subrun_state(AGENT_RESULT_STATUS_VERIFICATION_FAILED),
            SubRunState::VerificationFailed
        );
        assert_eq!(
            agent_result_status_to_subrun_state("mystery"),
            SubRunState::Failed
        );
    }

    #[test]
    fn agent_result_status_kind_normalizes_case_and_whitespace() {
        // Asymmetry-bug regression: ToolResultStatusKind::from_status_str
        // already lowercases + trims, but agent_result_status_kind used to be
        // exact-match. A producer that emitted "Completed" (capital C) or
        // "FAILED" silently fell into AgentResultStatusKind::Other and was
        // then projected to SubRunState::Failed — a successful sub-run was
        // reported as failed.
        for s in ["Completed", "COMPLETED", "  completed  ", "\tcompleted\n"] {
            assert_eq!(
                agent_result_status_kind(s),
                AgentResultStatusKind::Completed,
                "{s:?} must normalize to Completed",
            );
            assert!(agent_result_status_is_success(s), "{s:?} must be success");
            assert_eq!(
                agent_result_status_to_subrun_state(s),
                SubRunState::Completed,
                "{s:?} must project to Completed sub-run",
            );
        }
        for s in ["FAILED", "Failed"] {
            assert_eq!(agent_result_status_kind(s), AgentResultStatusKind::Failed);
            assert!(agent_result_status_is_failure(s));
        }
        for s in ["Verification_Failed", "VERIFICATION_FAILED"] {
            assert_eq!(
                agent_result_status_kind(s),
                AgentResultStatusKind::VerificationFailed
            );
        }
        for s in ["Paused", "WAITING"] {
            assert!(
                agent_result_status_is_unfinished(s),
                "{s:?} must be unfinished"
            );
        }
        // Truly unknown still falls through to Other.
        assert_eq!(
            agent_result_status_kind("mystery"),
            AgentResultStatusKind::Other
        );
    }

    #[test]
    fn delegation_result_status_helpers_match_from_results_projection() {
        let completed = DelegationResult::from_results("d1", vec![make_result("a1", "one")], None);
        let unfinished = DelegationResult::from_results(
            "d1.5",
            vec![make_result("a1", "one"), make_unfinished("a2")],
            None,
        );
        let partial = DelegationResult::from_results(
            "d2",
            vec![make_result("a1", "one"), make_failed("a2")],
            None,
        );
        let failed = DelegationResult::from_results("d3", vec![make_failed("a1")], None);

        assert_eq!(
            delegation_result_status_kind(&completed.status),
            DelegationResultStatusKind::Completed
        );
        assert_eq!(
            delegation_result_status_kind(&unfinished.status),
            DelegationResultStatusKind::Unfinished
        );
        assert!(unfinished.is_unfinished());
        assert_eq!(
            delegation_result_status_kind(&partial.status),
            DelegationResultStatusKind::Partial
        );
        assert_eq!(
            delegation_result_status_kind(&failed.status),
            DelegationResultStatusKind::Failed
        );

        let empty = DelegationResult::from_results("d4", vec![], None);
        assert_eq!(
            delegation_result_status_kind(&empty.status),
            DelegationResultStatusKind::Failed
        );
    }

    #[test]
    fn delegated_child_resolves_parent_to_completed() {
        // Regression: Delegated is a terminal-success status (maps to
        // SubRunState::Completed). A child that terminated as Delegated
        // must NOT classify the parent delegation as unfinished.
        let make_delegated = |agent_id: &str| AgentResult {
            agent_id: agent_id.into(),
            run_id: format!("run-{agent_id}"),
            status: AGENT_RESULT_STATUS_DELEGATED.into(),
            output: Some("nested delegation completed".into()),
            error: None,
            prompt_tokens: 100,
            completion_tokens: 50,
            tool_calls: 3,
        };

        // All-Delegated: parent should be Completed
        let all_delegated =
            DelegationResult::from_results("d-del", vec![make_delegated("a1")], None);
        assert_eq!(
            delegation_result_status_kind(&all_delegated.status),
            DelegationResultStatusKind::Completed,
            "Delegated children must aggregate to Completed, not Unfinished"
        );
        assert!(all_delegated.is_success());
        assert!(!all_delegated.is_unfinished());

        // Mixed completed + delegated: still all-success → Completed
        let mixed = DelegationResult::from_results(
            "d-mix",
            vec![make_result("a1", "one"), make_delegated("a2")],
            None,
        );
        assert_eq!(
            delegation_result_status_kind(&mixed.status),
            DelegationResultStatusKind::Completed,
        );
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
        for value in [
            serde_json::json!({"pattern":"adversarial_review","producer_id":"a","reviewer_id":"b","max_rounds":1,"timeout_sec":0,"acceptance_threshold":0.9}),
            serde_json::json!({"pattern":"pipeline","stages":[{"agent_id":"a","output_transform":"extract"}],"timeout_sec":0}),
        ] {
            assert!(serde_json::from_value::<CoordinationPattern>(value).is_err());
        }
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
            session_id: "test-session".into(),
            delegation_id: "d1".into(),
            parent_run_id: "run-1".into(),
            task: "test".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["w".into(), "r".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "u1".into(),
            depth: 1,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let restored: DelegationRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.delegation_id, "d1");
        assert_eq!(restored.depth, 1);
    }

    // ── suggest_pattern tests ──

    #[test]
    fn suggest_sequential_with_dependencies() {
        let hints = CoordinationHints {
            agent_ids: vec!["a1".into(), "a2".into(), "a3".into()],
            has_dependencies: true,
            timeout_sec: 30,
        };
        let pattern = suggest_pattern(&hints);
        assert!(
            matches!(pattern, CoordinationPattern::Sequential { .. }),
            "dependencies should yield Sequential"
        );
    }

    #[test]
    fn suggest_fanout_for_independent_agents() {
        let hints = CoordinationHints {
            agent_ids: vec!["a1".into(), "a2".into()],
            timeout_sec: 60,
            ..Default::default()
        };
        let pattern = suggest_pattern(&hints);
        assert!(
            matches!(pattern, CoordinationPattern::FanOut { .. }),
            "2+ independent agents should yield FanOut"
        );
    }

    #[test]
    fn suggest_single_agent_never_invents_fork_tasks() {
        let hints = CoordinationHints {
            agent_ids: vec!["a1".into()],
            timeout_sec: 60,
            ..Default::default()
        };
        let pattern = suggest_pattern(&hints);
        assert!(
            matches!(pattern, CoordinationPattern::Sequential { .. }),
            "typed hints cannot manufacture a Fork without an explicit task list"
        );
    }

    #[test]
    fn suggest_fallback_sequential_for_single_agent() {
        let hints = CoordinationHints {
            agent_ids: vec!["a1".into()],
            timeout_sec: 0,
            ..Default::default()
        };
        let pattern = suggest_pattern(&hints);
        assert!(
            matches!(pattern, CoordinationPattern::Sequential { .. }),
            "single agent + simple task should yield Sequential"
        );
    }

    #[test]
    fn suggest_does_not_infer_review_without_typed_hint() {
        let hints = CoordinationHints {
            agent_ids: vec!["a1".into(), "a2".into()],
            timeout_sec: 30,
            ..Default::default()
        };
        let pattern = suggest_pattern(&hints);
        assert!(
            matches!(pattern, CoordinationPattern::FanOut { .. }),
            "untyped prose cannot silently select an adversarial topology"
        );
    }

    #[test]
    fn suggest_empty_agents_returns_sequential() {
        let hints = CoordinationHints {
            agent_ids: vec![],
            timeout_sec: 30,
            ..Default::default()
        };
        let pattern = suggest_pattern(&hints);
        match pattern {
            CoordinationPattern::Sequential { agent_ids, .. } => {
                assert!(agent_ids.is_empty());
            }
            other => panic!("expected Sequential, got {:?}", other),
        }
    }
}
