//! Trait abstractions for team orchestration dependencies.
//!
//! These traits decouple `TeamExecutionOrchestrator` from concrete runtime types
//! (DelegationEngine, DelegationTracker, RunEngine) so the orchestrator can live
//! in `astra-server-types` while implementations stay in the runtime crate.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use astra_core::SubRunState;
use astra_services::coordination::{AgentProfileRegistry, DelegationRequest, DelegationResult};

// ─── Types that must live here for trait signatures ─────────────────────

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
    /// Lifecycle state (enforced state machine).
    pub state: SubRunState,
    /// If this run is a gate-retry, links to the original run_id.
    pub retry_of: Option<String>,
}

/// Real-time progress snapshot for an active delegation.
#[derive(Debug, Clone)]
pub struct DelegationProgress {
    pub delegation_id: String,
    /// Per-agent current state.
    pub agent_states: HashMap<String, SubRunState>,
    /// When execution started.
    pub started_at: std::time::Instant,
    /// Number of completed (terminal) sub-runs.
    pub completed_count: usize,
    /// Total sub-runs expected.
    pub total_count: usize,
}

// ─── Traits ─────────────────────────────────────────────────────────────────

/// Executes a multi-agent delegation and reports progress.
#[async_trait]
pub trait DelegationExecutor: Send + Sync {
    /// Execute a delegation request, returning the aggregated result.
    async fn execute_delegation(
        &self,
        request: DelegationRequest,
        source_agent_id: &str,
        profile_snapshot: AgentProfileRegistry,
        model_plan: Option<astra_turn_types::DirectDelegationModelPlan>,
        command_identity: Option<astra_turn_types::DirectDelegationCommandIdentity>,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    ) -> Result<DelegationResult, String>;

    /// Get real-time progress for an active delegation.
    async fn get_delegation_progress(&self, delegation_id: &str) -> Option<DelegationProgress>;
}

/// Tracks delegation hierarchies and pause state.
#[async_trait]
pub trait DelegationTracking: Send + Sync {
    /// Get all sub-runs for a delegation.
    async fn get_sub_runs(&self, delegation_id: &str) -> Vec<SubRunRecord>;

    /// Check if a sub-run is currently paused.
    async fn is_run_paused(&self, run_id: &str) -> bool;

    /// Pause all sub-runs in a delegation. Returns count paused.
    async fn pause_delegation(&self, delegation_id: &str) -> usize;

    /// Resume all sub-runs in a delegation. Returns count resumed.
    async fn resume_delegation(&self, delegation_id: &str) -> usize;

    /// Cleanup all state for a completed delegation.
    async fn cleanup_delegation(&self, delegation_id: &str) -> Result<(), String>;
}
