//! Trait abstractions for team orchestration dependencies.
//!
//! These traits decouple `TeamExecutionOrchestrator` from concrete runtime types
//! (DelegationEngine, DelegationTracker, RunEngine) so the orchestrator can live
//! in `astra-server-types` while implementations stay in the runtime crate.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use astra_core::SubRunState;
use astra_services::coordination::{DelegationRequest, DelegationResult};

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
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    ) -> Result<DelegationResult, String>;

    /// Get real-time progress for an active delegation.
    async fn get_delegation_progress(
        &self,
        delegation_id: &str,
    ) -> Option<DelegationProgress>;
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

/// Persists durable run state (events, status, checkpoints, usage).
#[async_trait]
pub trait RunPersistence: Send + Sync {
    /// Create a durable run record with delegation metadata.
    async fn start_run_ext(
        &self,
        run_id: &str,
        user_id: &str,
        session_id: &str,
        parent_run_id: Option<&str>,
        delegation_id: Option<&str>,
        agent_id: Option<&str>,
        retry_of: Option<&str>,
    ) -> Result<(), String>;

    /// Persist a status change.
    async fn persist_status(
        &self,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String>;

    /// Persist token/tool usage counters.
    async fn persist_usage(
        &self,
        run_id: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls: u32,
    ) -> Result<bool, String>;

    /// Save a checkpoint for crash recovery.
    async fn persist_checkpoint(
        &self,
        run_id: &str,
        checkpoint_json: &str,
    ) -> Result<bool, String>;

    /// Append an event to the durable event log.
    async fn append_event(&self, run_id: &str, event: Value) -> Result<(), String>;
}
