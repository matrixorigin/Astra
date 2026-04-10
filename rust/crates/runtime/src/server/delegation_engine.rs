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
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tokio::sync::RwLock;

use astra_services::coordination::{
    AgentProfile, AgentProfileRegistry, AgentResult, AggregationStrategy, CoordinationPattern,
    DelegationRequest, DelegationResult, aggregate_results,
};

pub use astra_core::SubRunState;
use astra_core::{
    InvalidTransition, STATUS_COMPLETED, STATUS_FAILED, STATUS_PAUSED, STATUS_RUNNING,
    STATUS_VERIFICATION_FAILED,
};

use super::run_engine::RunEngine;
use crate::messaging::router::AgentMailboxRouter;

// ─── Sub-run Executor Trait ─────────────────────────────────────────────────

/// Configuration for a sub-run spawned by delegation.
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
    /// Cooperative pause flag — checked between turns by the sub-run loop.
    /// When set to `true`, the sub-run should yield with status "paused".
    pub pause_flag: Option<Arc<AtomicBool>>,
    /// Mid-execution checkpoint gate — abort early if contract criteria are violated.
    pub checkpoint_gate: Option<Arc<dyn CheckpointGate>>,
    /// Optional mailbox for inter-agent messaging during the sub-run.
    pub mailbox: Option<crate::messaging::router::AgentMailbox>,
    /// Cancellation token — when cancelled, the sub-run should stop gracefully.
    pub cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
}

impl std::fmt::Debug for SubRunConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubRunConfig")
            .field("run_id", &self.run_id)
            .field("agent_profile", &self.agent_profile)
            .field("task", &self.task)
            .field("session_id", &self.session_id)
            .field("user_id", &self.user_id)
            .field("previous_output", &self.previous_output)
            .field("pause_flag", &self.pause_flag.is_some())
            .field("checkpoint_gate", &self.checkpoint_gate.is_some())
            .field("mailbox", &self.mailbox.is_some())
            .finish()
    }
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
            status: STATUS_COMPLETED.to_string(),
            output: Some(format!("[stub] completed task: {}", config.task)),
            error: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
        })
    }
}

// ─── Verification Gate ──────────────────────────────────────────────────────

/// Outcome of a verification gate check on a sub-run result.
#[derive(Debug, Clone)]
pub enum GateVerdict {
    /// Sub-run passed verification — proceed with aggregation.
    Pass,
    /// Sub-run failed verification — retry if attempts remain.
    Fail {
        reason: String,
        /// Verification details (criteria results, evidence, etc.)
        details: Option<serde_json::Value>,
    },
    /// Skip verification for this result (e.g., already failed sub-run).
    Skip,
}

impl GateVerdict {
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass | Self::Skip)
    }
}

/// Post-completion verification gate for delegation sub-runs.
///
/// Injected into [`DelegationEngine`] to validate sub-run output before aggregation.
/// When a gate returns [`GateVerdict::Fail`], the engine can retry the sub-run
/// (up to `max_retries`) or mark it as failed.
#[async_trait]
pub trait VerificationGate: Send + Sync {
    /// Verify a completed sub-run result.
    ///
    /// - `result`: the completed agent result
    /// - `delegation_id`: which delegation this belongs to
    /// - `attempt`: current attempt number (starts at 1)
    async fn verify(&self, result: &AgentResult, delegation_id: &str, attempt: u32) -> GateVerdict;

    /// Maximum retry attempts when verification fails. Default: 2.
    fn max_retries(&self) -> u32 {
        2
    }
}

// ─── Default Quality Gate ────────────────────────────────────────────────────

/// Configurable thresholds for [`DefaultQualityGate`].
#[derive(Debug, Clone)]
pub struct QualityThresholds {
    /// Minimum output length (chars). Default: 10.
    pub min_output_len: usize,
    /// Maximum output length (chars). Default: 50_000.
    pub max_output_len: usize,
    /// Maximum ratio of repeated lines to total lines (0.0–1.0). Default: 0.5.
    pub max_repetition_ratio: f64,
    /// Maximum number of retries. Default: 2.
    pub max_retries: u32,
}

impl Default for QualityThresholds {
    fn default() -> Self {
        Self {
            min_output_len: 10,
            max_output_len: 50_000,
            max_repetition_ratio: 0.5,
            max_retries: 2,
        }
    }
}

/// Production-ready verification gate with configurable heuristic checks.
///
/// Validates sub-run output quality:
/// - **Length bounds**: rejects empty/trivial or excessively long output
/// - **Repetition detection**: rejects output with >50% repeated lines (loop/garbage)
/// - **Error pattern detection**: rejects output dominated by error messages
pub struct DefaultQualityGate {
    thresholds: QualityThresholds,
}

impl DefaultQualityGate {
    pub fn new() -> Self {
        Self {
            thresholds: QualityThresholds::default(),
        }
    }

    pub fn with_thresholds(thresholds: QualityThresholds) -> Self {
        Self { thresholds }
    }
}

impl Default for DefaultQualityGate {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VerificationGate for DefaultQualityGate {
    async fn verify(
        &self,
        result: &AgentResult,
        _delegation_id: &str,
        _attempt: u32,
    ) -> GateVerdict {
        let output: &str = result.output.as_deref().unwrap_or("");

        // Check minimum length
        let trimmed_len = output.trim().len();
        if trimmed_len < self.thresholds.min_output_len {
            return GateVerdict::Fail {
                reason: format!(
                    "output too short ({} chars, minimum {})",
                    trimmed_len, self.thresholds.min_output_len
                ),
                details: Some(serde_json::json!({
                    "check": "min_length",
                    "actual": trimmed_len,
                    "threshold": self.thresholds.min_output_len
                })),
            };
        }

        // Check maximum length
        if output.len() > self.thresholds.max_output_len {
            return GateVerdict::Fail {
                reason: format!(
                    "output too long ({} chars, maximum {})",
                    output.len(),
                    self.thresholds.max_output_len
                ),
                details: Some(serde_json::json!({
                    "check": "max_length",
                    "actual": output.len(),
                    "threshold": self.thresholds.max_output_len
                })),
            };
        }

        // Repetition detection: count unique vs total non-empty lines
        let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.len() >= 4 {
            let unique: std::collections::HashSet<&str> = lines.iter().copied().collect();
            let repetition_ratio = 1.0 - (unique.len() as f64 / lines.len() as f64);
            if repetition_ratio > self.thresholds.max_repetition_ratio {
                return GateVerdict::Fail {
                    reason: format!(
                        "excessive repetition ({:.0}% repeated lines)",
                        repetition_ratio * 100.0
                    ),
                    details: Some(serde_json::json!({
                        "check": "repetition",
                        "total_lines": lines.len(),
                        "unique_lines": unique.len(),
                        "ratio": repetition_ratio
                    })),
                };
            }
        }

        // Error pattern detection: if >60% of lines are error-like, flag it
        if lines.len() >= 3 {
            let error_patterns = ["error:", "Error:", "ERROR", "panic", "FAILED", "fatal:"];
            let error_lines = lines
                .iter()
                .filter(|l| error_patterns.iter().any(|p| l.contains(p)))
                .count();
            let error_ratio = error_lines as f64 / lines.len() as f64;
            if error_ratio > 0.6 {
                return GateVerdict::Fail {
                    reason: format!(
                        "output dominated by errors ({:.0}% error lines)",
                        error_ratio * 100.0
                    ),
                    details: Some(serde_json::json!({
                        "check": "error_dominated",
                        "error_lines": error_lines,
                        "total_lines": lines.len(),
                        "ratio": error_ratio
                    })),
                };
            }
        }

        GateVerdict::Pass
    }

    fn max_retries(&self) -> u32 {
        self.thresholds.max_retries
    }
}

// ─── Checkpoint Gate (Mid-Execution Fail-Fast) ──────────────────────────────

/// Mid-execution checkpoint gate — checked between turns during a sub-run.
///
/// Unlike [`VerificationGate`] (which runs AFTER the sub-run completes),
/// a `CheckpointGate` is checked every N turns DURING execution. When it
/// returns `false`, the sub-run is aborted immediately, saving time on
/// clearly divergent executions.
///
/// Piggybacks on the existing cooperative-pause mechanism in the agentic loop.
#[async_trait]
pub trait CheckpointGate: Send + Sync {
    /// Called every `checkpoint_frequency()` turns during sub-run execution.
    ///
    /// Returns `true` to continue, `false` to abort.
    /// `turn_index` is the current turn number (0-based).
    /// `total_tool_calls` is the cumulative tool call count so far.
    async fn check(
        &self,
        run_id: &str,
        turn_index: u32,
        total_tool_calls: u32,
    ) -> Result<bool, String>;

    /// How many turns between checkpoint checks. Default: 3.
    fn checkpoint_frequency(&self) -> u32 {
        3
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

/// In-memory tracker for delegation hierarchies and pause state.
///
/// Optionally persists state changes to a session journal for crash recovery.
pub struct DelegationTracker {
    /// delegation_id → sub-run records
    delegations: RwLock<HashMap<String, Vec<SubRunRecord>>>,
    /// run_id → parent_run_id (for quick lookups)
    parents: RwLock<HashMap<String, String>>,
    /// run_id → cooperative pause flag (shared with the sub-run's loop)
    pause_flags: RwLock<HashMap<String, Arc<AtomicBool>>>,
    /// Optional session ID for journal persistence.
    session_id: Option<String>,
    /// Real-time progress per delegation.
    progress: RwLock<HashMap<String, DelegationProgress>>,
}

impl DelegationTracker {
    pub fn new() -> Self {
        Self {
            delegations: RwLock::new(HashMap::new()),
            parents: RwLock::new(HashMap::new()),
            pause_flags: RwLock::new(HashMap::new()),
            session_id: None,
            progress: RwLock::new(HashMap::new()),
        }
    }

    /// Create a tracker with journal persistence enabled.
    pub fn with_session(session_id: String) -> Self {
        Self {
            delegations: RwLock::new(HashMap::new()),
            parents: RwLock::new(HashMap::new()),
            pause_flags: RwLock::new(HashMap::new()),
            session_id: Some(session_id),
            progress: RwLock::new(HashMap::new()),
        }
    }

    /// Persist a delegation event to the session journal (best-effort).
    fn persist_event(
        &self,
        event_type: astra_services::session_journal::JournalEventType,
        metadata: serde_json::Value,
    ) {
        let Some(ref sid) = self.session_id else {
            return;
        };
        let writer = match astra_services::session_journal::JournalWriter::new(sid) {
            Ok(w) => w,
            Err(_) => return,
        };
        let mut event = astra_services::session_journal::JournalEvent::base_public(
            event_type,
            Some(sid.as_str()),
        );
        event.metadata = Some(metadata);
        let _ = writer.append(&event);
    }

    /// Rebuild in-memory hierarchy from durable run records.
    ///
    /// Called at startup to recover delegation state after a crash.
    /// Only records with `parent_run_id` set are considered (sub-runs).
    pub async fn load_from_run_records(&self, records: &[astra_services::runs::DurableRunRecord]) {
        let mut delegations = self.delegations.write().await;
        let mut parents = self.parents.write().await;
        let mut pause_flags = self.pause_flags.write().await;

        for rec in records {
            let (Some(parent_run_id), Some(delegation_id)) =
                (&rec.parent_run_id, &rec.delegation_id)
            else {
                continue; // Skip root runs
            };

            let sub = SubRunRecord {
                run_id: rec.run_id.clone(),
                parent_run_id: parent_run_id.clone(),
                delegation_id: delegation_id.clone(),
                agent_id: rec.agent_id.clone().unwrap_or_default(),
                depth: 0, // Depth not stored in DB; 0 is safe for recovered records
                state: SubRunState::from_str(&rec.status).unwrap_or(SubRunState::Failed),
                retry_of: None,
            };

            delegations
                .entry(delegation_id.clone())
                .or_default()
                .push(sub);
            parents.insert(rec.run_id.clone(), parent_run_id.clone());

            // Re-create pause flags for paused sub-runs
            if rec.status == STATUS_PAUSED {
                let flag = Arc::new(AtomicBool::new(true));
                pause_flags.insert(rec.run_id.clone(), flag);
            }
        }
    }

    /// Record a sub-run spawned by a delegation, persisting to journal if configured.
    pub async fn record_sub_run(&self, record: SubRunRecord) {
        let run_id = record.run_id.clone();
        let parent_id = record.parent_run_id.clone();
        let delegation_id = record.delegation_id.clone();

        // Persist to journal before updating in-memory state
        self.persist_event(
            astra_services::session_journal::JournalEventType::DelegationStarted,
            serde_json::json!({
                "delegation_id": &delegation_id,
                "run_id": &run_id,
                "parent_run_id": &parent_id,
                "agent_id": &record.agent_id,
                "depth": record.depth,
            }),
        );

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

    /// Get the recorded delegation depth for a run, if known.
    pub async fn get_depth(&self, run_id: &str) -> Option<u32> {
        for records in self.delegations.read().await.values() {
            for record in records {
                if record.run_id == run_id {
                    return Some(record.depth);
                }
            }
        }
        None
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

    // ── Pause / Resume ──────────────────────────────────────────────────────

    /// Register a cooperative pause flag for a sub-run.
    ///
    /// Returns the flag so the caller can pass it into [`SubRunConfig::pause_flag`].
    pub async fn register_pause_flag(&self, run_id: &str) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.pause_flags
            .write()
            .await
            .insert(run_id.to_string(), flag.clone());
        flag
    }

    /// Get the pause flag for a sub-run, if registered.
    pub async fn get_pause_flag(&self, run_id: &str) -> Option<Arc<AtomicBool>> {
        self.pause_flags.read().await.get(run_id).cloned()
    }

    /// Set the pause flag for a single sub-run.
    /// Returns `true` if the flag existed and was set.
    pub async fn pause_sub_run(&self, run_id: &str) -> bool {
        if let Some(flag) = self.pause_flags.read().await.get(run_id) {
            flag.store(true, Ordering::SeqCst);
            self.persist_event(
                astra_services::session_journal::JournalEventType::SyncMarker,
                serde_json::json!({ "action": "pause", "run_id": run_id }),
            );
            true
        } else {
            false
        }
    }

    /// Clear the pause flag for a single sub-run.
    /// Returns `true` if the flag existed and was cleared.
    pub async fn resume_sub_run(&self, run_id: &str) -> bool {
        if let Some(flag) = self.pause_flags.read().await.get(run_id) {
            flag.store(false, Ordering::SeqCst);
            self.persist_event(
                astra_services::session_journal::JournalEventType::SyncMarker,
                serde_json::json!({ "action": "resume", "run_id": run_id }),
            );
            true
        } else {
            false
        }
    }

    /// Pause ALL sub-runs belonging to a delegation.
    /// Returns the number of sub-runs paused.
    pub async fn pause_delegation(&self, delegation_id: &str) -> usize {
        let records = self.get_sub_runs(delegation_id).await;
        let flags = self.pause_flags.read().await;
        let mut count = 0;
        for record in &records {
            if let Some(flag) = flags.get(&record.run_id) {
                flag.store(true, Ordering::SeqCst);
                count += 1;
            }
        }
        count
    }

    /// Resume ALL sub-runs belonging to a delegation.
    /// Returns the number of sub-runs resumed.
    pub async fn resume_delegation(&self, delegation_id: &str) -> usize {
        let records = self.get_sub_runs(delegation_id).await;
        let flags = self.pause_flags.read().await;
        let mut count = 0;
        for record in &records {
            if let Some(flag) = flags.get(&record.run_id) {
                flag.store(false, Ordering::SeqCst);
                count += 1;
            }
        }
        count
    }

    /// Pause ALL sub-runs that have a given parent run ID.
    /// Returns the number of sub-runs paused.
    pub async fn pause_children_of(&self, parent_run_id: &str) -> usize {
        let children = self.get_children(parent_run_id).await;
        let flags = self.pause_flags.read().await;
        let mut count = 0;
        for child_id in &children {
            if let Some(flag) = flags.get(child_id) {
                flag.store(true, Ordering::SeqCst);
                count += 1;
            }
        }
        count
    }

    /// Resume ALL sub-runs that have a given parent run ID.
    /// Returns the number of sub-runs resumed.
    pub async fn resume_children_of(&self, parent_run_id: &str) -> usize {
        let children = self.get_children(parent_run_id).await;
        let flags = self.pause_flags.read().await;
        let mut count = 0;
        for child_id in &children {
            if let Some(flag) = flags.get(child_id) {
                flag.store(false, Ordering::SeqCst);
                count += 1;
            }
        }
        count
    }

    /// Check if a sub-run is currently paused.
    pub async fn is_paused(&self, run_id: &str) -> bool {
        self.pause_flags
            .read()
            .await
            .get(run_id)
            .is_some_and(|f| f.load(Ordering::Relaxed))
    }

    // ── State Machine + Lifecycle ───────────────────────────────────────────

    /// Transition a sub-run's state, enforcing the state machine.
    ///
    /// Returns `Err` if the transition is illegal.
    pub async fn transition_state(
        &self,
        run_id: &str,
        to: SubRunState,
    ) -> Result<SubRunState, InvalidTransition> {
        let mut delegations = self.delegations.write().await;
        for records in delegations.values_mut() {
            for record in records.iter_mut() {
                if record.run_id == run_id {
                    let new_state = record.state.try_transition(to)?;
                    record.state = new_state;

                    // Capture values before releasing the lock
                    let delegation_id = record.delegation_id.clone();
                    let agent_id = record.agent_id.clone();
                    drop(delegations);

                    // Update progress tracking
                    self.update_progress(&delegation_id, &agent_id, new_state)
                        .await;
                    return Ok(new_state);
                }
            }
        }
        // Run not tracked — allow the transition (e.g. root runs)
        Ok(to)
    }

    /// Mark a sub-run as complete: transition state, remove pause flag.
    ///
    /// Cleans up resources and updates progress tracking.
    pub async fn complete_sub_run(&self, run_id: &str, terminal_state: SubRunState) {
        debug_assert!(terminal_state.is_terminal());

        // Transition state in record
        let mut delegation_id = None;
        let mut agent_id = None;
        {
            let mut delegations = self.delegations.write().await;
            for records in delegations.values_mut() {
                for record in records.iter_mut() {
                    if record.run_id == run_id {
                        // Best-effort: if transition fails, force the terminal state
                        record.state = record
                            .state
                            .try_transition(terminal_state)
                            .unwrap_or(terminal_state);
                        delegation_id = Some(record.delegation_id.clone());
                        agent_id = Some(record.agent_id.clone());
                        break;
                    }
                }
                if delegation_id.is_some() {
                    break;
                }
            }
        }

        // Note: pause flags are NOT removed here — they are cleaned up
        // in cleanup_delegation() when the entire delegation completes.

        // Update progress
        if let (Some(did), Some(aid)) = (delegation_id, agent_id) {
            self.update_progress(&did, &aid, terminal_state).await;
        }
    }

    /// Bulk cleanup after a full delegation completes.
    ///
    /// Removes all pause flags for the delegation's sub-runs and
    /// cleans up the progress entry.
    pub async fn cleanup_delegation(&self, delegation_id: &str) {
        let records = self.get_sub_runs(delegation_id).await;
        let mut flags = self.pause_flags.write().await;
        for record in &records {
            flags.remove(&record.run_id);
        }
        drop(flags);

        self.progress.write().await.remove(delegation_id);
    }

    /// Get the full retry chain for a run: [original, retry1, retry2, ...]
    pub async fn get_retry_chain(&self, run_id: &str) -> Vec<String> {
        let delegations = self.delegations.read().await;

        // Find which delegation this run belongs to
        for records in delegations.values() {
            // First find the original (walk backward via retry_of)
            let mut original_id = run_id.to_string();
            loop {
                let found = records
                    .iter()
                    .find(|r| r.run_id == original_id && r.retry_of.is_some());
                match found {
                    Some(r) => original_id = r.retry_of.clone().unwrap(),
                    None => break,
                }
            }

            // Now collect forward: original → retries
            let mut chain = vec![original_id.clone()];
            let mut current = original_id;
            loop {
                let next = records
                    .iter()
                    .find(|r| r.retry_of.as_deref() == Some(&current));
                match next {
                    Some(r) => {
                        chain.push(r.run_id.clone());
                        current = r.run_id.clone();
                    }
                    None => break,
                }
            }
            if chain.len() > 1 || chain.first().map(|s| s.as_str()) == Some(run_id) {
                return chain;
            }
        }
        vec![run_id.to_string()]
    }

    // ── Progress Tracking ───────────────────────────────────────────────────

    /// Initialize progress tracking for a new delegation.
    pub async fn init_progress(&self, delegation_id: &str, agent_ids: &[String]) {
        let mut states = HashMap::new();
        for aid in agent_ids {
            states.insert(aid.clone(), SubRunState::Created);
        }
        self.progress.write().await.insert(
            delegation_id.to_string(),
            DelegationProgress {
                delegation_id: delegation_id.to_string(),
                agent_states: states,
                started_at: std::time::Instant::now(),
                completed_count: 0,
                total_count: agent_ids.len(),
            },
        );
    }

    /// Update an agent's state in the progress tracker.
    async fn update_progress(&self, delegation_id: &str, agent_id: &str, state: SubRunState) {
        let mut progress_map = self.progress.write().await;
        if let Some(progress) = progress_map.get_mut(delegation_id) {
            progress.agent_states.insert(agent_id.to_string(), state);
            progress.completed_count = progress
                .agent_states
                .values()
                .filter(|s| s.is_terminal())
                .count();
        }
    }

    /// Get a snapshot of delegation progress.
    pub async fn get_progress(&self, delegation_id: &str) -> Option<DelegationProgress> {
        self.progress.read().await.get(delegation_id).cloned()
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
    /// Optional post-completion verification gate.
    gate: Option<Arc<dyn VerificationGate>>,
    /// Optional mailbox router for inter-agent messaging.
    mailbox_router: Option<Arc<AgentMailboxRouter>>,
}

impl DelegationEngine {
    pub fn new(
        registry: Arc<RwLock<AgentProfileRegistry>>,
        run_engine: Arc<RunEngine>,
        tracker: Arc<DelegationTracker>,
    ) -> Self {
        #[cfg(debug_assertions)]
        eprintln!(
            "  ⚠ DelegationEngine: using StubSubRunExecutor — call with_executor() for production"
        );
        Self {
            registry,
            run_engine,
            tracker,
            executor: Arc::new(StubSubRunExecutor),
            gate: None,
            mailbox_router: None,
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
            gate: None,
            mailbox_router: None,
        }
    }

    /// Attach a verification gate. Sub-run results will be checked before aggregation.
    pub fn with_gate(mut self, gate: Arc<dyn VerificationGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Attach a mailbox router for inter-agent messaging within delegations.
    pub fn with_mailbox_router(mut self, router: Arc<AgentMailboxRouter>) -> Self {
        self.mailbox_router = Some(router);
        self
    }

    /// Dynamically set the verification gate (e.g., per-subtask criteria during plan execution).
    ///
    /// Unlike [`with_gate`] (builder pattern), this mutates the engine in place so callers
    /// can swap gates between delegation calls without rebuilding the engine.
    pub fn set_gate(&mut self, gate: Arc<dyn VerificationGate>) {
        self.gate = Some(gate);
    }

    /// Remove the current verification gate (sub-runs will bypass verification).
    pub fn clear_gate(&mut self) {
        self.gate = None;
    }

    /// Create a new engine sharing the same components but with a different gate.
    ///
    /// All `Arc`-wrapped internals (registry, run_engine, tracker, executor) are
    /// cheaply cloned (pointer bumps).  Use this when the engine is behind an
    /// `Arc` and `set_gate` cannot be called because `&mut self` is unavailable.
    pub fn clone_with_gate(&self, gate: Arc<dyn VerificationGate>) -> Self {
        Self {
            registry: self.registry.clone(),
            run_engine: self.run_engine.clone(),
            tracker: self.tracker.clone(),
            executor: self.executor.clone(),
            gate: Some(gate),
            mailbox_router: self.mailbox_router.clone(),
        }
    }

    /// Clone sharing all Arc components but with the gate cleared.
    /// Used between subtasks to prevent a previous subtask's gate from leaking.
    pub fn clone_without_gate(&self) -> Self {
        Self {
            registry: self.registry.clone(),
            run_engine: self.run_engine.clone(),
            tracker: self.tracker.clone(),
            executor: self.executor.clone(),
            gate: None,
            mailbox_router: self.mailbox_router.clone(),
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

    /// Apply the verification gate to a sub-run result with retry support.
    ///
    /// Returns the final result after gate checking (possibly retried).
    /// If no gate is configured, returns the result as-is.
    async fn apply_gate(
        &self,
        result: AgentResult,
        delegation_id: &str,
        config_builder: impl Fn() -> SubRunConfig,
    ) -> AgentResult {
        let gate = match &self.gate {
            Some(g) => g,
            None => return result,
        };

        // Skip gate for already-failed results
        if !result.is_success() {
            return result;
        }

        let max_retries = gate.max_retries();
        let mut current = result;
        let mut attempt = 1u32;

        loop {
            match gate.verify(&current, delegation_id, attempt).await {
                GateVerdict::Pass | GateVerdict::Skip => return current,
                GateVerdict::Fail { reason, details } => {
                    // Persist retry count to durable store for crash recovery
                    astra_core::log_persist!(
                        self.run_engine
                            .persist_retry_count(&current.run_id, attempt)
                            .await,
                        "delegation",
                        &current.run_id,
                        "retry_count"
                    );

                    // Record the gate failure in run events
                    astra_core::log_persist!(
                        self.run_engine
                            .append_event(
                                &current.run_id,
                                serde_json::json!({
                                    "event_type": "verification_gate_failed",
                                    "data": {
                                        "attempt": attempt,
                                        "reason": reason,
                                        "details": details,
                                    }
                                }),
                            )
                            .await,
                        "delegation",
                        &current.run_id,
                        "gate_failed_event"
                    );

                    if attempt >= max_retries {
                        // Exhausted retries — mark as verification failure
                        astra_core::log_persist!(
                            self.run_engine
                                .persist_status(
                                    &current.run_id,
                                    STATUS_VERIFICATION_FAILED,
                                    None,
                                    Some(&reason),
                                )
                                .await,
                            "delegation",
                            &current.run_id,
                            "verification_failed"
                        );
                        return AgentResult {
                            status: STATUS_VERIFICATION_FAILED.to_string(),
                            error: Some(format!(
                                "verification gate failed after {attempt} attempts: {reason}"
                            )),
                            ..current
                        };
                    }

                    // Retry: re-execute with the same config
                    attempt += 1;
                    let retry_config = config_builder();
                    match self.executor.execute(retry_config).await {
                        Ok(r) => {
                            astra_core::log_persist!(
                                self.run_engine
                                    .persist_status(&r.run_id, &r.status, None, r.error.as_deref())
                                    .await,
                                "delegation",
                                &r.run_id,
                                "status"
                            );
                            current = r;
                        }
                        Err(e) => {
                            return AgentResult {
                                status: STATUS_FAILED.to_string(),
                                error: Some(format!("retry execution failed: {e}")),
                                ..current
                            };
                        }
                    }
                }
            }
        }
    }

    /// Execute a delegation: spawn sub-runs according to the coordination pattern.
    ///
    /// Returns a `DelegationResult` with individual agent results and
    /// aggregated output. Sub-runs are created in the RunEngine and tracked
    /// in the DelegationTracker for hierarchy queries.
    ///
    /// `cancel_token` is scoped to this execution — no global state. When
    /// cancelled, all spawned sub-runs receive the signal and stop gracefully.
    pub async fn execute(
        &self,
        request: DelegationRequest,
        source_agent_id: &str,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    ) -> Result<DelegationResult, String> {
        // Validate first
        self.validate(&request, source_agent_id).await?;

        let session_id = request
            .context
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("delegation");

        // Extract pattern name and agent_ids for journal event.
        let (pattern_name, agent_ids_for_journal): (&str, Vec<String>) = match &request.pattern {
            CoordinationPattern::FanOut { agent_ids, .. } => ("fan_out", agent_ids.clone()),
            CoordinationPattern::Pipeline { stages } => (
                "pipeline",
                stages.iter().map(|s| s.agent_id.clone()).collect(),
            ),
            CoordinationPattern::Sequential { agent_ids, .. } => ("sequential", agent_ids.clone()),
            CoordinationPattern::AdversarialReview {
                producer_id,
                reviewer_id,
                ..
            } => (
                "adversarial_review",
                vec![producer_id.clone(), reviewer_id.clone()],
            ),
            CoordinationPattern::Fork {
                agent_id, tasks, ..
            } => ("fork", vec![format!("{}×{}", agent_id, tasks.len())]),
        };

        // Journal: delegation started
        Self::write_journal_event(
            session_id,
            astra_services::session_journal::JournalEvent::delegation_started(
                Some(session_id),
                &request.delegation_id,
                &request.parent_run_id,
                pattern_name,
                &agent_ids_for_journal,
            ),
        );

        // Initialize progress tracking
        self.tracker
            .init_progress(&request.delegation_id, &agent_ids_for_journal)
            .await;

        let result = match &request.pattern {
            CoordinationPattern::FanOut {
                agent_ids,
                aggregation,
                ..
            } => {
                self.execute_fan_out(&request, agent_ids, aggregation, cancel_token.as_ref())
                    .await
            }
            CoordinationPattern::Pipeline { stages } => {
                let agent_ids: Vec<String> = stages.iter().map(|s| s.agent_id.clone()).collect();
                self.execute_sequential(&request, &agent_ids, false, cancel_token.as_ref())
                    .await
            }
            CoordinationPattern::Sequential {
                agent_ids,
                stop_on_success,
            } => {
                self.execute_sequential(
                    &request,
                    agent_ids,
                    *stop_on_success,
                    cancel_token.as_ref(),
                )
                .await
            }
            CoordinationPattern::AdversarialReview {
                producer_id,
                reviewer_id,
                max_rounds,
                ..
            } => {
                self.execute_adversarial(
                    &request,
                    producer_id,
                    reviewer_id,
                    *max_rounds,
                    cancel_token.as_ref(),
                )
                .await
            }
            CoordinationPattern::Fork {
                tasks,
                agent_id,
                max_turns,
                aggregation,
            } => {
                self.execute_fork(
                    &request,
                    tasks,
                    agent_id,
                    *max_turns,
                    aggregation,
                    cancel_token.as_ref(),
                )
                .await
            }
        };

        // Journal: sub-run completions + delegation completed
        if let Ok(ref dr) = result {
            for ar in &dr.agent_results {
                Self::write_journal_event(
                    session_id,
                    astra_services::session_journal::JournalEvent::delegation_sub_run_completed(
                        Some(session_id),
                        &request.delegation_id,
                        &ar.run_id,
                        &ar.agent_id,
                        &ar.status,
                        ar.error.as_deref(),
                    ),
                );
            }
            let succeeded = dr.agent_results.iter().filter(|r| r.is_success()).count();
            let failed = dr.agent_results.len() - succeeded;
            Self::write_journal_event(
                session_id,
                astra_services::session_journal::JournalEvent::delegation_completed(
                    Some(session_id),
                    &request.delegation_id,
                    pattern_name,
                    dr.agent_results.len(),
                    succeeded,
                    failed,
                    &dr.status,
                ),
            );
        }

        result
    }

    /// Write a journal event (best-effort, non-blocking).
    fn write_journal_event(session_id: &str, event: astra_services::session_journal::JournalEvent) {
        if let Ok(writer) = astra_services::session_journal::JournalWriter::new(session_id) {
            let _ = writer.append(&event);
        }
    }

    /// Fan-out: spawn all agents in parallel, aggregate results.
    async fn execute_fan_out(
        &self,
        request: &DelegationRequest,
        agent_ids: &[String],
        aggregation: &AggregationStrategy,
        cancel_token: Option<&Arc<tokio_util::sync::CancellationToken>>,
    ) -> Result<DelegationResult, String> {
        let reg = self.registry.read().await;

        // Build configs + create runs in parallel
        let mut configs = Vec::new();
        for agent_id in agent_ids {
            let sub_run_id = uuid::Uuid::new_v4().to_string();
            let session_id = request
                .context
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("delegation");

            self.run_engine
                .start_run_ext(
                    &sub_run_id,
                    &request.user_id,
                    session_id,
                    Some(&request.parent_run_id),
                    Some(&request.delegation_id),
                    Some(agent_id),
                )
                .await?;

            self.tracker
                .record_sub_run(SubRunRecord {
                    run_id: sub_run_id.clone(),
                    parent_run_id: request.parent_run_id.clone(),
                    delegation_id: request.delegation_id.clone(),
                    agent_id: agent_id.clone(),
                    depth: request.depth + 1,
                    state: SubRunState::Created,
                    retry_of: None,
                })
                .await;

            // Transition Created → Running
            let _ = self
                .tracker
                .transition_state(&sub_run_id, SubRunState::Running)
                .await;

            self.run_engine
                .persist_status(&sub_run_id, STATUS_RUNNING, Some("agent_execution"), None)
                .await?;

            let pause_flag = self.tracker.register_pause_flag(&sub_run_id).await;

            let profile = reg.get(agent_id).cloned().unwrap_or_else(|| {
                AgentProfile::new(
                    agent_id,
                    agent_id,
                    astra_services::coordination::AgentTier::User,
                )
            });

            // Register with mailbox router and obtain a mailbox handle (if router available).
            let mailbox = if let Some(router) = &self.mailbox_router {
                let addr = crate::messaging::types::AgentAddress {
                    run_id: sub_run_id.clone(),
                    agent_id: agent_id.clone(),
                };
                match router
                    .register(addr, Some(request.delegation_id.clone()))
                    .await
                {
                    Ok(mb) => Some(mb),
                    Err(e) => {
                        eprintln!(
                            "  ⚠ delegation: mailbox registration failed for {agent_id}: {e}"
                        );
                        None
                    }
                }
            } else {
                None
            };

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
                pause_flag: Some(pause_flag),
                checkpoint_gate: None,
                mailbox,
                cancel_token: cancel_token.cloned(),
            });
        }
        drop(reg);

        // Execute sub-runs in parallel, respecting optional max_parallel limit.
        let max_parallel = request
            .context
            .get("team_max_parallel")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let semaphore = if max_parallel > 0 {
            Some(Arc::new(tokio::sync::Semaphore::new(max_parallel)))
        } else {
            None
        };

        let mut handles: Vec<(
            tokio::task::JoinHandle<(Result<AgentResult, String>, String, String)>,
            String,
            String,
        )> = Vec::new();
        for config in configs {
            let executor = self.executor.clone();
            let run_engine = self.run_engine.clone();
            let tracker = self.tracker.clone();
            let sem = semaphore.clone();
            let cancel = cancel_token.cloned();
            // Capture identity before moving config into the closure (panic context)
            let captured_agent_id = config.agent_profile.agent_id.clone();
            let captured_run_id = config.run_id.clone();
            let handle = tokio::spawn(async move {
                let _permit = match sem {
                    Some(ref s) => Some(s.acquire().await.expect("semaphore closed")),
                    None => None,
                };
                let run_id = config.run_id.clone();
                let agent_id = config.agent_profile.agent_id.clone();
                let result = if let Some(ref token) = cancel {
                    tokio::select! {
                        r = executor.execute(config) => r,
                        _ = token.cancelled() => Err("cancelled by budget timeout".to_string()),
                    }
                } else {
                    executor.execute(config).await
                };
                // Determine final state and persist
                let final_state = match &result {
                    Ok(r) => {
                        astra_core::log_persist!(
                            run_engine
                                .persist_status(&run_id, &r.status, None, r.error.as_deref())
                                .await,
                            "delegation",
                            &run_id,
                            "status"
                        );
                        SubRunState::from_str(&r.status).unwrap_or(SubRunState::Failed)
                    }
                    Err(e) => {
                        astra_core::log_persist!(
                            run_engine
                                .persist_status(&run_id, STATUS_FAILED, None, Some(e.as_str()))
                                .await,
                            "delegation",
                            &run_id,
                            "status"
                        );
                        SubRunState::Failed
                    }
                };
                tracker.complete_sub_run(&run_id, final_state).await;
                (result, agent_id, run_id)
            });
            handles.push((handle, captured_agent_id, captured_run_id));
        }

        let mut results = Vec::new();
        for (handle, panic_agent_id, panic_run_id) in handles {
            match handle.await {
                Ok((Ok(result), _, _)) => results.push(result),
                Ok((Err(e), agent_id, run_id)) => {
                    results.push(AgentResult {
                        agent_id,
                        run_id,
                        status: STATUS_FAILED.to_string(),
                        output: None,
                        error: Some(e),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    });
                }
                Err(e) => {
                    // JoinError (panic) — use captured identity instead of "unknown"
                    astra_core::log_persist!(
                        self.run_engine
                            .persist_status(
                                &panic_run_id,
                                STATUS_FAILED,
                                None,
                                Some(&format!("task panicked: {e}"))
                            )
                            .await,
                        "delegation",
                        &panic_run_id,
                        "status"
                    );
                    self.tracker
                        .complete_sub_run(&panic_run_id, SubRunState::Failed)
                        .await;
                    results.push(AgentResult {
                        agent_id: panic_agent_id,
                        run_id: panic_run_id,
                        status: STATUS_FAILED.to_string(),
                        output: None,
                        error: Some(format!("task join error (panic): {}", e)),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    });
                }
            }
        }

        // ── Verification gate: check each result before aggregation ──
        if self.gate.is_some() {
            let delegation_id = request.delegation_id.clone();
            let mut gated_results = Vec::with_capacity(results.len());
            for result in results {
                let did = delegation_id.clone();
                // Fan-out gate is check-only (no retry — configs are consumed).
                // For retry support, use Sequential pattern instead.
                let gated = self
                    .apply_gate(result, &did, || {
                        // No-retry stub: return a dummy config that won't actually be called
                        // because max_retries check fires first in the closure.
                        SubRunConfig {
                            run_id: String::new(),
                            agent_profile: AgentProfile::new(
                                "stub",
                                "stub",
                                astra_services::coordination::AgentTier::User,
                            ),
                            task: String::new(),
                            session_id: String::new(),
                            user_id: String::new(),
                            previous_output: None,
                            context: HashMap::new(),
                            pause_flag: None,
                            checkpoint_gate: None,
                            mailbox: None,
                            cancel_token: None,
                        }
                    })
                    .await;
                gated_results.push(gated);
            }
            results = gated_results;
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
        cancel_token: Option<&Arc<tokio_util::sync::CancellationToken>>,
    ) -> Result<DelegationResult, String> {
        let reg = self.registry.read().await;
        let mut results = Vec::new();
        let mut previous_output: Option<String> = None;

        for agent_id in agent_ids {
            // Check cancellation before starting next sequential agent
            if let Some(ref token) = cancel_token {
                if token.is_cancelled() {
                    break;
                }
            }

            let sub_run_id = uuid::Uuid::new_v4().to_string();
            let session_id = request
                .context
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("delegation");

            self.run_engine
                .start_run_ext(
                    &sub_run_id,
                    &request.user_id,
                    session_id,
                    Some(&request.parent_run_id),
                    Some(&request.delegation_id),
                    Some(agent_id),
                )
                .await?;

            self.tracker
                .record_sub_run(SubRunRecord {
                    run_id: sub_run_id.clone(),
                    parent_run_id: request.parent_run_id.clone(),
                    delegation_id: request.delegation_id.clone(),
                    agent_id: agent_id.clone(),
                    depth: request.depth + 1,
                    state: SubRunState::Created,
                    retry_of: None,
                })
                .await;

            // Transition Created → Running
            let _ = self
                .tracker
                .transition_state(&sub_run_id, SubRunState::Running)
                .await;

            self.run_engine
                .persist_status(&sub_run_id, STATUS_RUNNING, Some("agent_execution"), None)
                .await?;

            let pause_flag = self.tracker.register_pause_flag(&sub_run_id).await;

            let profile = reg.get(agent_id).cloned().unwrap_or_else(|| {
                AgentProfile::new(
                    agent_id,
                    agent_id,
                    astra_services::coordination::AgentTier::User,
                )
            });

            let mailbox = if let Some(router) = &self.mailbox_router {
                let addr = crate::messaging::types::AgentAddress {
                    run_id: sub_run_id.clone(),
                    agent_id: agent_id.clone(),
                };
                match router
                    .register(addr, Some(request.delegation_id.clone()))
                    .await
                {
                    Ok(mb) => Some(mb),
                    Err(e) => {
                        eprintln!(
                            "  ⚠ delegation: mailbox registration failed for {agent_id}: {e}"
                        );
                        None
                    }
                }
            } else {
                None
            };

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
                pause_flag: Some(pause_flag),
                checkpoint_gate: None,
                mailbox,
                cancel_token: cancel_token.cloned(),
            };

            let result = match self.executor.execute(config).await {
                Ok(r) => {
                    astra_core::log_persist!(
                        self.run_engine
                            .persist_status(&sub_run_id, &r.status, None, r.error.as_deref())
                            .await,
                        "delegation",
                        &sub_run_id,
                        "status"
                    );
                    let final_state =
                        SubRunState::from_str(&r.status).unwrap_or(SubRunState::Failed);
                    self.tracker
                        .complete_sub_run(&sub_run_id, final_state)
                        .await;
                    r
                }
                Err(e) => {
                    astra_core::log_persist!(
                        self.run_engine
                            .persist_status(&sub_run_id, STATUS_FAILED, None, Some(e.as_str()))
                            .await,
                        "delegation",
                        &sub_run_id,
                        "status"
                    );
                    self.tracker
                        .complete_sub_run(&sub_run_id, SubRunState::Failed)
                        .await;
                    AgentResult {
                        agent_id: agent_id.clone(),
                        run_id: sub_run_id,
                        status: STATUS_FAILED.to_string(),
                        output: None,
                        error: Some(e),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    }
                }
            };

            // ── Verification gate with retry for sequential sub-runs ──
            let result = if self.gate.is_some() {
                let delegation_id = request.delegation_id.clone();
                let task = request.task.clone();
                let sess = request
                    .context
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("delegation")
                    .to_string();
                let uid = request.user_id.clone();
                let ctx = request.context.clone();
                let prev = previous_output.clone();
                let profile_for_retry = reg.get(agent_id).cloned().unwrap_or_else(|| {
                    AgentProfile::new(
                        agent_id,
                        agent_id,
                        astra_services::coordination::AgentTier::User,
                    )
                });
                self.apply_gate(result, &delegation_id, || SubRunConfig {
                    run_id: uuid::Uuid::new_v4().to_string(),
                    agent_profile: profile_for_retry.clone(),
                    task: task.clone(),
                    session_id: sess.clone(),
                    user_id: uid.clone(),
                    previous_output: prev.clone(),
                    context: ctx.clone(),
                    pause_flag: None,
                    checkpoint_gate: None,
                    mailbox: None,
                    cancel_token: None,
                })
                .await
            } else {
                result
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
        cancel_token: Option<&Arc<tokio_util::sync::CancellationToken>>,
    ) -> Result<DelegationResult, String> {
        let reg = self.registry.read().await;
        let mut results = Vec::new();
        let mut last_producer_output: Option<String> = None;

        let producer_profile = reg.get(producer_id).cloned().unwrap_or_else(|| {
            AgentProfile::new(
                producer_id,
                producer_id,
                astra_services::coordination::AgentTier::System,
            )
        });
        let reviewer_profile = reg.get(reviewer_id).cloned().unwrap_or_else(|| {
            AgentProfile::new(
                reviewer_id,
                reviewer_id,
                astra_services::coordination::AgentTier::System,
            )
        });
        drop(reg);

        for round in 0..max_rounds {
            // Check cancellation before starting next adversarial round
            if let Some(ref token) = cancel_token {
                if token.is_cancelled() {
                    break;
                }
            }

            // ── Producer sub-run ──
            let prod_run_id = uuid::Uuid::new_v4().to_string();
            let session_id = request
                .context
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("delegation");
            self.run_engine
                .start_run_ext(
                    &prod_run_id,
                    &request.user_id,
                    session_id,
                    Some(&request.parent_run_id),
                    Some(&request.delegation_id),
                    Some(producer_id),
                )
                .await?;
            self.tracker
                .record_sub_run(SubRunRecord {
                    run_id: prod_run_id.clone(),
                    parent_run_id: request.parent_run_id.clone(),
                    delegation_id: request.delegation_id.clone(),
                    agent_id: producer_id.to_string(),
                    depth: request.depth + 1,
                    state: SubRunState::Created,
                    retry_of: None,
                })
                .await;
            let _ = self
                .tracker
                .transition_state(&prod_run_id, SubRunState::Running)
                .await;
            self.run_engine
                .persist_status(&prod_run_id, STATUS_RUNNING, Some("produce"), None)
                .await?;
            let prod_pause = self.tracker.register_pause_flag(&prod_run_id).await;
            self.run_engine
                .append_event(
                    &prod_run_id,
                    serde_json::json!({"event_type": "adversarial_round", "data": {"round": round, "role": "producer"}}),
                )
                .await?;

            let prod_mailbox = if let Some(router) = &self.mailbox_router {
                let addr = crate::messaging::types::AgentAddress {
                    run_id: prod_run_id.clone(),
                    agent_id: producer_id.to_string(),
                };
                match router
                    .register(addr, Some(request.delegation_id.clone()))
                    .await
                {
                    Ok(mb) => Some(mb),
                    Err(e) => {
                        eprintln!(
                            "  ⚠ delegation: mailbox registration failed for {producer_id}: {e}"
                        );
                        None
                    }
                }
            } else {
                None
            };

            let prod_config = SubRunConfig {
                run_id: prod_run_id.clone(),
                agent_profile: producer_profile.clone(),
                task: request.task.clone(),
                session_id: request
                    .context
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("delegation")
                    .to_string(),
                user_id: request.user_id.clone(),
                previous_output: last_producer_output.clone(),
                context: request.context.clone(),
                pause_flag: Some(prod_pause.clone()),
                checkpoint_gate: None,
                mailbox: prod_mailbox,
                cancel_token: cancel_token.cloned(),
            };
            let prod_result = match self.executor.execute(prod_config).await {
                Ok(r) => {
                    astra_core::log_persist!(
                        self.run_engine
                            .persist_status(&prod_run_id, &r.status, None, r.error.as_deref())
                            .await,
                        "delegation",
                        &prod_run_id,
                        "status"
                    );
                    let final_state =
                        SubRunState::from_str(&r.status).unwrap_or(SubRunState::Failed);
                    self.tracker
                        .complete_sub_run(&prod_run_id, final_state)
                        .await;
                    r
                }
                Err(e) => {
                    astra_core::log_persist!(
                        self.run_engine
                            .persist_status(&prod_run_id, STATUS_FAILED, None, Some(e.as_str()))
                            .await,
                        "delegation",
                        &prod_run_id,
                        "status"
                    );
                    self.tracker
                        .complete_sub_run(&prod_run_id, SubRunState::Failed)
                        .await;
                    AgentResult {
                        agent_id: producer_id.to_string(),
                        run_id: prod_run_id,
                        status: STATUS_FAILED.to_string(),
                        output: None,
                        error: Some(e),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    }
                }
            };

            // ── Gate on producer output before reviewer sees it ──
            let prod_result = if self.gate.is_some() {
                let did = request.delegation_id.clone();
                let task = request.task.clone();
                let sess = request
                    .context
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("delegation")
                    .to_string();
                let uid = request.user_id.clone();
                let ctx = request.context.clone();
                let prev = last_producer_output.clone();
                let pp = producer_profile.clone();
                self.apply_gate(prod_result, &did, || SubRunConfig {
                    run_id: uuid::Uuid::new_v4().to_string(),
                    agent_profile: pp.clone(),
                    task: task.clone(),
                    session_id: sess.clone(),
                    user_id: uid.clone(),
                    previous_output: prev.clone(),
                    context: ctx.clone(),
                    pause_flag: None,
                    checkpoint_gate: None,
                    mailbox: None,
                    cancel_token: None,
                })
                .await
            } else {
                prod_result
            };

            last_producer_output = prod_result.output.clone();
            results.push(prod_result);

            // ── Reviewer sub-run ──
            let rev_run_id = uuid::Uuid::new_v4().to_string();
            self.run_engine
                .start_run_ext(
                    &rev_run_id,
                    &request.user_id,
                    session_id,
                    Some(&request.parent_run_id),
                    Some(&request.delegation_id),
                    Some(reviewer_id),
                )
                .await?;
            self.tracker
                .record_sub_run(SubRunRecord {
                    run_id: rev_run_id.clone(),
                    parent_run_id: request.parent_run_id.clone(),
                    delegation_id: request.delegation_id.clone(),
                    agent_id: reviewer_id.to_string(),
                    depth: request.depth + 1,
                    state: SubRunState::Created,
                    retry_of: None,
                })
                .await;
            let _ = self
                .tracker
                .transition_state(&rev_run_id, SubRunState::Running)
                .await;
            self.run_engine
                .persist_status(&rev_run_id, STATUS_RUNNING, Some("review"), None)
                .await?;
            let rev_pause = self.tracker.register_pause_flag(&rev_run_id).await;
            self.run_engine
                .append_event(
                    &rev_run_id,
                    serde_json::json!({"event_type": "adversarial_round", "data": {"round": round, "role": "reviewer"}}),
                )
                .await?;

            let rev_mailbox = if let Some(router) = &self.mailbox_router {
                let addr = crate::messaging::types::AgentAddress {
                    run_id: rev_run_id.clone(),
                    agent_id: reviewer_id.to_string(),
                };
                match router
                    .register(addr, Some(request.delegation_id.clone()))
                    .await
                {
                    Ok(mb) => Some(mb),
                    Err(e) => {
                        eprintln!(
                            "  ⚠ delegation: mailbox registration failed for {reviewer_id}: {e}"
                        );
                        None
                    }
                }
            } else {
                None
            };

            let rev_config = SubRunConfig {
                run_id: rev_run_id.clone(),
                agent_profile: reviewer_profile.clone(),
                task: format!(
                    "Review this output:\n\n{}",
                    last_producer_output.as_deref().unwrap_or("[no output]")
                ),
                session_id: request
                    .context
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("delegation")
                    .to_string(),
                user_id: request.user_id.clone(),
                previous_output: last_producer_output.clone(),
                context: request.context.clone(),
                pause_flag: Some(rev_pause),
                checkpoint_gate: None,
                mailbox: rev_mailbox,
                cancel_token: cancel_token.cloned(),
            };
            let rev_result = match self.executor.execute(rev_config).await {
                Ok(r) => {
                    astra_core::log_persist!(
                        self.run_engine
                            .persist_status(&rev_run_id, &r.status, None, r.error.as_deref())
                            .await,
                        "delegation",
                        &rev_run_id,
                        "status"
                    );
                    let final_state =
                        SubRunState::from_str(&r.status).unwrap_or(SubRunState::Failed);
                    self.tracker
                        .complete_sub_run(&rev_run_id, final_state)
                        .await;
                    r
                }
                Err(e) => {
                    astra_core::log_persist!(
                        self.run_engine
                            .persist_status(&rev_run_id, STATUS_FAILED, None, Some(e.as_str()))
                            .await,
                        "delegation",
                        &rev_run_id,
                        "status"
                    );
                    self.tracker
                        .complete_sub_run(&rev_run_id, SubRunState::Failed)
                        .await;
                    AgentResult {
                        agent_id: reviewer_id.to_string(),
                        run_id: rev_run_id,
                        status: STATUS_FAILED.to_string(),
                        output: None,
                        error: Some(e),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
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

    /// Fork: dispatch N tasks sharing the parent's full conversation context.
    ///
    /// All fork children receive the same message prefix (the parent's conversation
    /// history up to this point), enabling prompt cache sharing across children.
    /// Fork children cannot recursively fork or delegate.
    async fn execute_fork(
        &self,
        request: &DelegationRequest,
        tasks: &[String],
        agent_id: &str,
        _max_turns: u32,
        _aggregation: &AggregationStrategy,
        cancel_token: Option<&Arc<tokio_util::sync::CancellationToken>>,
    ) -> Result<DelegationResult, String> {
        let reg = self.registry.read().await;
        let profile = reg.get(agent_id).cloned().unwrap_or_else(|| {
            AgentProfile::new(
                agent_id,
                agent_id,
                astra_services::coordination::AgentTier::User,
            )
        });
        drop(reg);

        // Extract parent messages for context inheritance (if provided)
        let parent_messages = request
            .context
            .get("parent_messages")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([]));

        let session_id = request
            .context
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("delegation");

        // Spawn fork children in parallel, respecting optional max_parallel limit.
        let max_parallel = request
            .context
            .get("team_max_parallel")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let fork_semaphore = if max_parallel > 0 {
            Some(Arc::new(tokio::sync::Semaphore::new(max_parallel)))
        } else {
            None
        };
        let mut handles: Vec<(
            tokio::task::JoinHandle<(Result<AgentResult, String>, String, String)>,
            String,
            String,
        )> = Vec::with_capacity(tasks.len());
        for (i, task) in tasks.iter().enumerate() {
            let run_id = uuid::Uuid::new_v4().to_string();
            let _ = self
                .run_engine
                .start_run_ext(
                    &run_id,
                    &request.user_id,
                    session_id,
                    Some(&request.parent_run_id),
                    Some(&request.delegation_id),
                    Some(agent_id),
                )
                .await;
            self.tracker
                .record_sub_run(SubRunRecord {
                    run_id: run_id.clone(),
                    parent_run_id: request.parent_run_id.clone(),
                    delegation_id: request.delegation_id.clone(),
                    agent_id: agent_id.to_string(),
                    depth: request.depth + 1,
                    state: SubRunState::Created,
                    retry_of: None,
                })
                .await;
            let _ = self
                .tracker
                .transition_state(&run_id, SubRunState::Running)
                .await;
            let _ = self
                .run_engine
                .persist_status(&run_id, "running", Some("fork"), None)
                .await;
            let pause_flag = self.tracker.register_pause_flag(&run_id).await;

            let fork_mailbox = if let Some(router) = &self.mailbox_router {
                let addr = crate::messaging::types::AgentAddress {
                    run_id: run_id.clone(),
                    agent_id: agent_id.to_string(),
                };
                match router
                    .register(addr, Some(request.delegation_id.clone()))
                    .await
                {
                    Ok(mb) => Some(mb),
                    Err(e) => {
                        eprintln!(
                            "  ⚠ delegation: mailbox registration failed for {agent_id}: {e}"
                        );
                        None
                    }
                }
            } else {
                None
            };

            // Build fork-specific context: parent messages + fork instruction
            let mut fork_context = request.context.clone();
            fork_context.insert("fork_index".to_string(), serde_json::json!(i));
            fork_context.insert("parent_messages".to_string(), parent_messages.clone());
            fork_context.insert("is_fork_child".to_string(), serde_json::json!(true));

            let fork_task = format!(
                "You are fork child #{i} of {total}.\n\
                 Task: {task}\n\n\
                 Rules:\n\
                 - Do NOT fork or delegate to other agents.\n\
                 - Execute the task directly and report results.\n\
                 - Be concise in your output.",
                i = i,
                total = tasks.len(),
                task = task,
            );

            let mut fork_profile = profile.clone();
            fork_profile.can_delegate = false;
            fork_profile.max_delegation_depth = 0;

            let config = SubRunConfig {
                run_id: run_id.clone(),
                agent_profile: fork_profile,
                task: fork_task,
                session_id: session_id.to_string(),
                user_id: request.user_id.clone(),
                previous_output: None,
                context: fork_context,
                pause_flag: Some(pause_flag),
                checkpoint_gate: None,
                mailbox: fork_mailbox,
                cancel_token: cancel_token.cloned(),
            };

            let executor = self.executor.clone();
            let run_engine = self.run_engine.clone();
            let tracker = self.tracker.clone();
            let sem = fork_semaphore.clone();
            let cancel_for_spawn = cancel_token.cloned();
            // Capture identity before moving config (panic context)
            let captured_agent_id = config.agent_profile.agent_id.clone();
            let captured_run_id = config.run_id.clone();
            let handle = tokio::spawn(async move {
                let _permit = match sem {
                    Some(ref s) => Some(s.acquire().await.expect("semaphore closed")),
                    None => None,
                };
                let run_id = config.run_id.clone();
                let agent_id = config.agent_profile.agent_id.clone();
                let result = if let Some(token) = cancel_for_spawn {
                    tokio::select! {
                        r = executor.execute(config) => r,
                        _ = token.cancelled() => {
                            Err("cancelled by budget timeout".to_string())
                        }
                    }
                } else {
                    executor.execute(config).await
                };
                let final_state = match &result {
                    Ok(r) => {
                        let _ = run_engine
                            .persist_status(&run_id, &r.status, None, r.error.as_deref())
                            .await;
                        SubRunState::from_str(&r.status).unwrap_or(SubRunState::Failed)
                    }
                    Err(e) => {
                        let _ = run_engine
                            .persist_status(&run_id, "failed", None, Some(e))
                            .await;
                        SubRunState::Failed
                    }
                };
                tracker.complete_sub_run(&run_id, final_state).await;
                (result, agent_id, run_id)
            });
            handles.push((handle, captured_agent_id, captured_run_id));
        }

        // Collect all results
        let mut results = Vec::with_capacity(handles.len());
        for (handle, panic_agent_id, panic_run_id) in handles {
            match handle.await {
                Ok((Ok(r), _, _)) => results.push(r),
                Ok((Err(e), agent_id_from_task, run_id_from_task)) => results.push(AgentResult {
                    agent_id: agent_id_from_task,
                    run_id: run_id_from_task,
                    status: "failed".to_string(),
                    output: None,
                    error: Some(e),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                }),
                Err(e) => {
                    // JoinError (panic) — use captured identity
                    let _ = self
                        .run_engine
                        .persist_status(
                            &panic_run_id,
                            STATUS_FAILED,
                            None,
                            Some(&format!("fork task panicked: {e}")),
                        )
                        .await;
                    self.tracker
                        .complete_sub_run(&panic_run_id, SubRunState::Failed)
                        .await;
                    results.push(AgentResult {
                        agent_id: panic_agent_id,
                        run_id: panic_run_id,
                        status: "failed".to_string(),
                        output: None,
                        error: Some(format!("fork task panicked: {}", e)),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    });
                }
            }
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

    /// Get the shared profile registry.
    pub fn registry(&self) -> &Arc<RwLock<AgentProfileRegistry>> {
        &self.registry
    }

    /// Get the shared run engine.
    pub fn run_engine(&self) -> &Arc<RunEngine> {
        &self.run_engine
    }

    // ── Pause / Resume API ──────────────────────────────────────────────────

    /// Pause all sub-runs belonging to a delegation.
    ///
    /// Sets cooperative pause flags — sub-runs check these between turns and
    /// yield with status "paused" at the next turn boundary.
    pub async fn pause_delegation(&self, delegation_id: &str) -> usize {
        let count = self.tracker.pause_delegation(delegation_id).await;
        // Persist pause status for each sub-run
        for record in self.tracker.get_sub_runs(delegation_id).await {
            astra_core::log_persist!(
                self.run_engine
                    .persist_status(
                        &record.run_id,
                        STATUS_PAUSED,
                        Some("delegation_pause"),
                        None
                    )
                    .await,
                "delegation",
                &record.run_id,
                "pause"
            );
        }
        count
    }

    /// Resume all sub-runs belonging to a delegation.
    ///
    /// Clears cooperative pause flags so sub-runs continue executing.
    pub async fn resume_delegation(&self, delegation_id: &str) -> usize {
        let count = self.tracker.resume_delegation(delegation_id).await;
        for record in self.tracker.get_sub_runs(delegation_id).await {
            astra_core::log_persist!(
                self.run_engine
                    .persist_status(
                        &record.run_id,
                        STATUS_RUNNING,
                        Some("delegation_resume"),
                        None
                    )
                    .await,
                "delegation",
                &record.run_id,
                "resume"
            );
        }
        count
    }

    /// Pause all sub-runs spawned by a parent run (across all delegations).
    pub async fn pause_children_of(&self, parent_run_id: &str) -> usize {
        let count = self.tracker.pause_children_of(parent_run_id).await;
        for child_id in self.tracker.get_children(parent_run_id).await {
            astra_core::log_persist!(
                self.run_engine
                    .persist_status(&child_id, STATUS_PAUSED, Some("parent_pause"), None)
                    .await,
                "delegation",
                &child_id,
                "pause"
            );
        }
        count
    }

    /// Resume all sub-runs spawned by a parent run.
    pub async fn resume_children_of(&self, parent_run_id: &str) -> usize {
        let count = self.tracker.resume_children_of(parent_run_id).await;
        for child_id in self.tracker.get_children(parent_run_id).await {
            astra_core::log_persist!(
                self.run_engine
                    .persist_status(&child_id, STATUS_RUNNING, Some("parent_resume"), None)
                    .await,
                "delegation",
                &child_id,
                "resume"
            );
        }
        count
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::coordination::{AgentProfile, AgentTier, PipelineStage};
    use astra_services::runs::{InMemoryRunStateStore, RunStateStore};

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
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "sub-2".into(),
                parent_run_id: "parent-1".into(),
                delegation_id: "del-1".into(),
                agent_id: "reviewer".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
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
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "grandchild".into(),
                parent_run_id: "child".into(),
                delegation_id: "d2".into(),
                agent_id: "b".into(),
                depth: 2,
                state: SubRunState::Created,
                retry_of: None,
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
        let result = de.execute(req, "orch", None).await.unwrap();

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

        let result = de.execute(req, "orch", None).await.unwrap();
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

        let result = de.execute(req, "orch", None).await.unwrap();
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

        let result = de.execute(req, "orch", None).await.unwrap();
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

        assert!(de.execute(req, "writer", None).await.is_err());
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

        let err = de.execute(req, "orch", None).await.unwrap_err();
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

        de.execute(req1, "orch", None).await.unwrap();
        de.execute(req2, "orch", None).await.unwrap();

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
                format!(
                    "[{}] {}: prev={}",
                    config.agent_profile.agent_id, config.task, prev
                )
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

    fn setup_with_executor(
        executor: Arc<dyn SubRunExecutor>,
    ) -> (
        Arc<RwLock<AgentProfileRegistry>>,
        Arc<RunEngine>,
        Arc<DelegationTracker>,
        DelegationEngine,
    ) {
        let (reg, engine, tracker) = setup();
        let de =
            DelegationEngine::with_executor(reg.clone(), engine.clone(), tracker.clone(), executor);
        (reg, engine, tracker, de)
    }

    #[tokio::test]
    async fn fan_out_executes_with_custom_executor() {
        let (_, engine, tracker, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = fan_out_request(vec!["coder", "reviewer"]);
        let result = de.execute(req, "orch", None).await.unwrap();

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

        let result = de.execute(req, "orch", None).await.unwrap();
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

        let result = de.execute(req, "orch", None).await.unwrap();
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
        let result = de.execute(req, "orch", None).await.unwrap();

        assert_eq!(result.status, "partial");
        assert_eq!(result.agent_results.len(), 2);

        let coder = result
            .agent_results
            .iter()
            .find(|r| r.agent_id == "coder")
            .unwrap();
        assert_eq!(coder.status, "completed");
        assert!(coder.output.is_some());

        let reviewer = result
            .agent_results
            .iter()
            .find(|r| r.agent_id == "reviewer")
            .unwrap();
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

        let result = de.execute(req, "orch", None).await.unwrap();
        // 2 rounds × (producer + reviewer) = 4
        assert_eq!(result.agent_results.len(), 4);
        assert_eq!(result.status, "completed");

        // All agents produced output
        for ar in &result.agent_results {
            assert!(ar.output.is_some());
        }

        // Token aggregation across all sub-runs
        assert_eq!(result.total_prompt_tokens, 40); // 4 × 10
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
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

        assert_eq!(
            tracker.get_agent_id("sub-1").await,
            Some("coder".to_string())
        );
        assert_eq!(tracker.get_agent_id("parent").await, None);
        assert_eq!(tracker.get_agent_id("nonexistent").await, None);
    }

    #[tokio::test]
    async fn with_executor_constructor_uses_custom_executor() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(EchoExecutor));

        let req = fan_out_request(vec!["coder"]);
        let result = de.execute(req, "orch", None).await.unwrap();
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
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                })
            }
        }

        let (reg, engine, tracker) = setup();
        let de =
            DelegationEngine::with_executor(reg, engine, tracker, Arc::new(ContextCheckExecutor));

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

        let result = de.execute(req, "orch", None).await.unwrap();
        assert_eq!(
            result.agent_results[0].output.as_deref(),
            Some("context_present=true")
        );
    }

    #[tokio::test]
    async fn worktree_path_per_agent_flows_through_context() {
        /// Executor that captures the agent-specific worktree_path from context.
        struct WorktreeCheckExecutor;

        #[async_trait]
        impl SubRunExecutor for WorktreeCheckExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
                let key = format!("worktree_path_{}", config.agent_profile.agent_id);
                let path = config
                    .context
                    .get(&key)
                    .and_then(|v| v.as_str())
                    .unwrap_or("none")
                    .to_string();
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some(path),
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                })
            }
        }

        let (reg, engine, tracker) = setup();
        // Register two agents
        {
            let mut r = reg.write().await;
            let _ = r.register(AgentProfile::new("agent-a", "Agent A", AgentTier::User));
            let _ = r.register(AgentProfile::new("agent-b", "Agent B", AgentTier::User));
        }
        let de =
            DelegationEngine::with_executor(reg, engine, tracker, Arc::new(WorktreeCheckExecutor));

        let mut ctx = HashMap::new();
        ctx.insert(
            "worktree_path_agent-a".to_string(),
            serde_json::json!("/tmp/wt/agent-a"),
        );
        ctx.insert(
            "worktree_path_agent-b".to_string(),
            serde_json::json!("/tmp/wt/agent-b"),
        );

        let req = DelegationRequest {
            delegation_id: "wt-test".into(),
            parent_run_id: "p".into(),
            task: "check worktree".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["agent-a".into(), "agent-b".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 30,
            },
            user_id: "u".into(),
            depth: 0,
            context: ctx,
        };

        let result = de.execute(req, "orch", None).await.unwrap();
        assert_eq!(result.agent_results.len(), 2);

        // Each agent should see its own worktree path
        for ar in &result.agent_results {
            let expected_path = format!("/tmp/wt/{}", ar.agent_id);
            assert_eq!(
                ar.output.as_deref(),
                Some(expected_path.as_str()),
                "agent {} should see its worktree path",
                ar.agent_id
            );
        }
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
            pause_flag: None,
            checkpoint_gate: None,
            mailbox: None,
            cancel_token: None,
        };

        let result = executor.execute(config).await.unwrap();
        assert_eq!(result.status, "completed");
        assert!(result.output.unwrap().contains("hello world"));
    }

    #[tokio::test]
    async fn pause_children_of_sets_flags_and_persists() {
        let (_, engine, tracker, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = fan_out_request(vec!["coder", "reviewer"]);
        let result = de.execute(req, "orch", None).await.unwrap();
        assert_eq!(result.agent_results.len(), 2);

        // Pause all children of parent-1
        let paused = de.pause_children_of("parent-1").await;
        assert_eq!(paused, 2);

        // Verify flags are set
        for ar in &result.agent_results {
            assert!(tracker.is_paused(&ar.run_id).await);
            let run = engine.load_run(&ar.run_id).await.unwrap().unwrap();
            assert_eq!(run.status, "paused");
        }

        // Resume all children
        let resumed = de.resume_children_of("parent-1").await;
        assert_eq!(resumed, 2);

        for ar in &result.agent_results {
            assert!(!tracker.is_paused(&ar.run_id).await);
            let run = engine.load_run(&ar.run_id).await.unwrap().unwrap();
            assert_eq!(run.status, "running");
        }
    }

    #[tokio::test]
    async fn pause_delegation_by_id_sets_flags() {
        let (_, engine, tracker, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = fan_out_request(vec!["coder", "reviewer"]);
        de.execute(req, "orch", None).await.unwrap();

        let paused = de.pause_delegation("del-1").await;
        assert_eq!(paused, 2);

        let subs = tracker.get_sub_runs("del-1").await;
        for sub in &subs {
            assert!(tracker.is_paused(&sub.run_id).await);
            let run = engine.load_run(&sub.run_id).await.unwrap().unwrap();
            assert_eq!(run.status, "paused");
        }

        let resumed = de.resume_delegation("del-1").await;
        assert_eq!(resumed, 2);

        for sub in &subs {
            assert!(!tracker.is_paused(&sub.run_id).await);
            let run = engine.load_run(&sub.run_id).await.unwrap().unwrap();
            assert_eq!(run.status, "running");
        }
    }

    // ─── Verification Gate Tests ────────────────────────────────────────────

    /// Gate that always passes.
    struct AlwaysPassGate;

    #[async_trait]
    impl VerificationGate for AlwaysPassGate {
        async fn verify(
            &self,
            _result: &AgentResult,
            _delegation_id: &str,
            _attempt: u32,
        ) -> GateVerdict {
            GateVerdict::Pass
        }
    }

    /// Gate that fails the first N attempts, then passes.
    struct FailThenPassGate {
        fail_count: std::sync::atomic::AtomicU32,
        max_fails: u32,
    }

    impl FailThenPassGate {
        fn new(max_fails: u32) -> Self {
            Self {
                fail_count: std::sync::atomic::AtomicU32::new(0),
                max_fails,
            }
        }
    }

    #[async_trait]
    impl VerificationGate for FailThenPassGate {
        async fn verify(
            &self,
            _result: &AgentResult,
            _delegation_id: &str,
            _attempt: u32,
        ) -> GateVerdict {
            let count = self.fail_count.fetch_add(1, Ordering::Relaxed);
            if count < self.max_fails {
                GateVerdict::Fail {
                    reason: format!("fail #{}", count + 1),
                    details: None,
                }
            } else {
                GateVerdict::Pass
            }
        }

        fn max_retries(&self) -> u32 {
            3
        }
    }

    /// Gate that always fails.
    struct AlwaysFailGate;

    #[async_trait]
    impl VerificationGate for AlwaysFailGate {
        async fn verify(
            &self,
            _result: &AgentResult,
            _delegation_id: &str,
            _attempt: u32,
        ) -> GateVerdict {
            GateVerdict::Fail {
                reason: "quality too low".into(),
                details: Some(serde_json::json!({"score": 0.3})),
            }
        }

        fn max_retries(&self) -> u32 {
            2
        }
    }

    #[tokio::test]
    async fn gate_pass_does_not_alter_results() {
        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(EchoExecutor));
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(EchoExecutor))
            .with_gate(Arc::new(AlwaysPassGate));

        let req = fan_out_request(vec!["coder", "reviewer"]);
        let result = de.execute(req, "orch", None).await.unwrap();

        assert_eq!(result.status, "completed");
        assert_eq!(result.agent_results.len(), 2);
        for ar in &result.agent_results {
            assert_eq!(ar.status, "completed");
        }
    }

    #[tokio::test]
    async fn gate_fail_marks_verification_failed() {
        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(EchoExecutor));
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(EchoExecutor))
            .with_gate(Arc::new(AlwaysFailGate));

        let req = fan_out_request(vec!["coder"]);
        let result = de.execute(req, "orch", None).await.unwrap();

        // Fan-out with always-fail gate: result should be verification_failed
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, "verification_failed");
        assert!(
            result.agent_results[0]
                .error
                .as_ref()
                .unwrap()
                .contains("quality too low")
        );
    }

    #[tokio::test]
    async fn gate_retry_then_pass_sequential() {
        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(EchoExecutor));
        // Fail once, then pass on second attempt
        let gate = Arc::new(FailThenPassGate::new(1));
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(EchoExecutor))
            .with_gate(gate);

        let req = DelegationRequest {
            delegation_id: "del-seq-gate".into(),
            parent_run_id: "parent-1".into(),
            task: "sequential gate test".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
            },
            user_id: "user-1".into(),
            depth: 0,
            context: HashMap::new(),
        };
        let result = de.execute(req, "orch", None).await.unwrap();

        // Should eventually pass after retry
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, "completed");
    }

    #[tokio::test]
    async fn gate_exhausted_retries_sequential() {
        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(EchoExecutor));
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(EchoExecutor))
            .with_gate(Arc::new(AlwaysFailGate));

        let req = DelegationRequest {
            delegation_id: "del-seq-fail".into(),
            parent_run_id: "parent-1".into(),
            task: "will fail".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
            },
            user_id: "user-1".into(),
            depth: 0,
            context: HashMap::new(),
        };
        let result = de.execute(req, "orch", None).await.unwrap();

        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, "verification_failed");
    }

    #[tokio::test]
    async fn no_gate_is_backward_compatible() {
        // Without gate, everything works as before
        let (_, _, _, de) = setup_with_executor(Arc::new(EchoExecutor));
        // de has no gate

        let req = fan_out_request(vec!["coder", "reviewer"]);
        let result = de.execute(req, "orch", None).await.unwrap();

        assert_eq!(result.status, "completed");
        assert_eq!(result.agent_results.len(), 2);
        for ar in &result.agent_results {
            assert_eq!(ar.status, "completed");
        }
    }

    #[tokio::test]
    async fn gate_skips_failed_subrun() {
        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(FailingExecutor {
            fail_agents: vec!["coder".into()],
        }));
        // AlwaysFailGate should NOT apply to already-failed results
        let de = DelegationEngine::with_executor(
            reg,
            engine,
            tracker,
            Arc::new(FailingExecutor {
                fail_agents: vec!["coder".into()],
            }),
        )
        .with_gate(Arc::new(AlwaysFailGate));

        let req = fan_out_request(vec!["coder"]);
        let result = de.execute(req, "orch", None).await.unwrap();

        // Should be "failed" (from executor), NOT "verification_failed"
        assert_eq!(result.agent_results[0].status, "failed");
    }

    #[tokio::test]
    async fn gate_verdict_variants() {
        assert!(GateVerdict::Pass.is_pass());
        assert!(GateVerdict::Skip.is_pass());
        assert!(
            !GateVerdict::Fail {
                reason: "x".into(),
                details: None
            }
            .is_pass()
        );
    }

    // ── Persistence tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn start_run_ext_persists_delegation_metadata() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());

        engine
            .start_run_ext(
                "sub-1",
                "user-1",
                "sess-1",
                Some("parent-1"),
                Some("del-1"),
                Some("coder"),
            )
            .await
            .unwrap();

        let record = store.load_run("sub-1").await.unwrap().unwrap();
        assert_eq!(record.parent_run_id.as_deref(), Some("parent-1"));
        assert_eq!(record.delegation_id.as_deref(), Some("del-1"));
        assert_eq!(record.agent_id.as_deref(), Some("coder"));
        assert_eq!(record.session_id, "sess-1");
    }

    #[tokio::test]
    async fn start_run_backward_compat_sets_none() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());

        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();

        let record = store.load_run("run-1").await.unwrap().unwrap();
        assert!(record.parent_run_id.is_none());
        assert!(record.delegation_id.is_none());
        assert!(record.agent_id.is_none());
    }

    #[tokio::test]
    async fn find_sub_runs_by_delegation_id() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());

        // Create a root run and two sub-runs in different delegations
        engine.start_run("root", "u1", "s1").await.unwrap();
        engine
            .start_run_ext(
                "sub-a",
                "u1",
                "s1",
                Some("root"),
                Some("del-1"),
                Some("coder"),
            )
            .await
            .unwrap();
        engine
            .start_run_ext(
                "sub-b",
                "u1",
                "s1",
                Some("root"),
                Some("del-1"),
                Some("reviewer"),
            )
            .await
            .unwrap();
        engine
            .start_run_ext(
                "sub-c",
                "u1",
                "s1",
                Some("root"),
                Some("del-2"),
                Some("writer"),
            )
            .await
            .unwrap();

        let del1_runs = engine.find_sub_runs("del-1").await.unwrap();
        assert_eq!(del1_runs.len(), 2);

        let del2_runs = engine.find_sub_runs("del-2").await.unwrap();
        assert_eq!(del2_runs.len(), 1);
        assert_eq!(del2_runs[0].agent_id.as_deref(), Some("writer"));
    }

    #[tokio::test]
    async fn persist_and_read_retry_count() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());

        engine.start_run("run-1", "u1", "s1").await.unwrap();
        assert_eq!(
            store.load_run("run-1").await.unwrap().unwrap().retry_count,
            0
        );

        engine.persist_retry_count("run-1", 2).await.unwrap();
        assert_eq!(
            store.load_run("run-1").await.unwrap().unwrap().retry_count,
            2
        );
    }

    #[tokio::test]
    async fn load_from_run_records_rebuilds_tracker() {
        use astra_services::runs::DurableRunRecord;

        let records = vec![
            DurableRunRecord {
                run_id: "sub-1".into(),
                user_id: "u1".into(),
                session_id: "s1".into(),
                parent_run_id: Some("parent-1".into()),
                delegation_id: Some("del-1".into()),
                agent_id: Some("coder".into()),
                status: "completed".into(),
                waiting_for: None,
                checkpoint_json: None,
                error_message: None,
                retry_count: 0,
                total_prompt_tokens: 0,
                total_completion_tokens: 0,
                total_tool_calls: 0,
                events: vec![],
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
            },
            DurableRunRecord {
                run_id: "sub-2".into(),
                user_id: "u1".into(),
                session_id: "s1".into(),
                parent_run_id: Some("parent-1".into()),
                delegation_id: Some("del-1".into()),
                agent_id: Some("reviewer".into()),
                status: "paused".into(),
                waiting_for: None,
                checkpoint_json: None,
                error_message: None,
                retry_count: 1,
                total_prompt_tokens: 0,
                total_completion_tokens: 0,
                total_tool_calls: 0,
                events: vec![],
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
            },
            // Root run — should be skipped
            DurableRunRecord {
                run_id: "root-run".into(),
                user_id: "u1".into(),
                session_id: "s1".into(),
                parent_run_id: None,
                delegation_id: None,
                agent_id: None,
                status: "completed".into(),
                waiting_for: None,
                checkpoint_json: None,
                error_message: None,
                retry_count: 0,
                total_prompt_tokens: 0,
                total_completion_tokens: 0,
                total_tool_calls: 0,
                events: vec![],
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
            },
        ];

        let tracker = DelegationTracker::new();
        tracker.load_from_run_records(&records).await;

        // Hierarchy rebuilt
        let subs = tracker.get_sub_runs("del-1").await;
        assert_eq!(subs.len(), 2);
        assert!(tracker.is_sub_run("sub-1").await);
        assert!(tracker.is_sub_run("sub-2").await);
        assert!(!tracker.is_sub_run("root-run").await);

        // Parent links rebuilt
        assert_eq!(
            tracker.get_parent("sub-1").await.as_deref(),
            Some("parent-1")
        );
        assert_eq!(
            tracker.get_agent_id("sub-1").await.as_deref(),
            Some("coder")
        );

        // Paused sub-run gets pause flag
        let flag = tracker.get_pause_flag("sub-2").await;
        assert!(flag.is_some());
        assert!(flag.unwrap().load(Ordering::SeqCst)); // paused = true

        // Completed sub-run has no pause flag
        assert!(tracker.get_pause_flag("sub-1").await.is_none());
    }

    // ─── clone_with_gate ─────────────────────────────────────────────────

    #[test]
    fn clone_with_gate_shares_components() {
        let run_store = Arc::new(astra_services::runs::InMemoryRunStateStore::default());
        let registry = Arc::new(tokio::sync::RwLock::new(AgentProfileRegistry::new()));
        let run_engine = Arc::new(crate::server::run_engine::RunEngine::new(run_store));
        let tracker = Arc::new(DelegationTracker::new());
        let executor: Arc<dyn SubRunExecutor> = Arc::new(StubSubRunExecutor);

        let engine = DelegationEngine::with_executor(
            registry.clone(),
            run_engine.clone(),
            tracker.clone(),
            executor.clone(),
        );
        assert!(engine.gate.is_none());

        // Clone with a gate — the new engine shares the same Arc components.
        struct PassGate;
        #[async_trait::async_trait]
        impl VerificationGate for PassGate {
            async fn verify(&self, _: &AgentResult, _: &str, _: u32) -> GateVerdict {
                GateVerdict::Pass
            }
        }

        let gated = engine.clone_with_gate(Arc::new(PassGate));
        assert!(gated.gate.is_some());
    }

    // ─── Fork Pattern Tests ─────────────────────────────────────────────

    fn fork_request(del_id: &str, tasks: Vec<&str>, agent_id: &str) -> DelegationRequest {
        DelegationRequest {
            delegation_id: del_id.into(),
            parent_run_id: format!("parent-{del_id}"),
            task: "fork test".into(),
            pattern: CoordinationPattern::Fork {
                tasks: tasks.into_iter().map(String::from).collect(),
                agent_id: agent_id.into(),
                max_turns: 5,
                aggregation: AggregationStrategy::AllResults,
            },
            user_id: "user-1".into(),
            depth: 0,
            context: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn fork_spawns_parallel_children() {
        let (_, _engine, tracker, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = fork_request(
            "del-fork-spawn",
            vec!["task-a", "task-b", "task-c"],
            "writer",
        );
        let result = de.execute(req, "orch", None).await.unwrap();

        assert_eq!(result.agent_results.len(), 3);
        assert_eq!(result.status, "completed");

        let subs = tracker.get_sub_runs("del-fork-spawn").await;
        assert_eq!(subs.len(), 3);
        for sub in &subs {
            assert_eq!(sub.agent_id, "writer");
            assert_eq!(sub.depth, 1);
        }

        // All results should have output
        for ar in &result.agent_results {
            assert_eq!(ar.status, "completed");
            assert!(ar.output.is_some());
        }
    }

    #[tokio::test]
    async fn fork_children_cannot_delegate() {
        /// Executor that checks can_delegate is false on fork children.
        struct DelegateCheckExecutor;

        #[async_trait]
        impl SubRunExecutor for DelegateCheckExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
                let can_del = config.agent_profile.can_delegate;
                let depth = config.agent_profile.max_delegation_depth;
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some(format!("can_delegate={can_del},depth={depth}")),
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                })
            }
        }

        let (reg, engine, tracker) = setup();
        let de =
            DelegationEngine::with_executor(reg, engine, tracker, Arc::new(DelegateCheckExecutor));

        let req = fork_request("del-fork-deleg", vec!["task-a"], "writer");
        let result = de.execute(req, "orch", None).await.unwrap();

        assert_eq!(
            result.agent_results[0].output.as_deref(),
            Some("can_delegate=false,depth=0")
        );
    }

    #[tokio::test]
    async fn fork_partial_failure() {
        let executor = Arc::new(FailingExecutor {
            fail_agents: vec!["writer".to_string()],
        });
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(reg, engine, tracker, executor);

        let req = fork_request("del-fork-fail", vec!["task-a", "task-b"], "writer");
        let result = de.execute(req, "orch", None).await.unwrap();

        // All children use "writer" which fails → all failed
        assert_eq!(result.agent_results.len(), 2);
        assert_eq!(result.status, "failed");
        for ar in &result.agent_results {
            assert_eq!(ar.status, "failed");
        }
    }

    #[tokio::test]
    async fn fork_single_task() {
        let (_, _, _, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = fork_request("del-fork-single", vec!["only-task"], "writer");
        let result = de.execute(req, "orch", None).await.unwrap();

        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.status, "completed");
    }

    #[tokio::test]
    async fn fork_context_includes_fork_metadata() {
        /// Executor that checks fork context fields.
        struct ForkContextCheckExecutor;

        #[async_trait]
        impl SubRunExecutor for ForkContextCheckExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
                let is_fork = config
                    .context
                    .get("is_fork_child")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let idx = config
                    .context
                    .get("fork_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(999);
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some(format!("is_fork={is_fork},idx={idx}")),
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                })
            }
        }

        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(
            reg,
            engine,
            tracker,
            Arc::new(ForkContextCheckExecutor),
        );

        let req = fork_request("del-fork-ctx", vec!["a", "b"], "writer");
        let result = de.execute(req, "orch", None).await.unwrap();

        // Both children should have fork metadata
        let outputs: Vec<String> = result
            .agent_results
            .iter()
            .filter_map(|r| r.output.clone())
            .collect();
        assert!(outputs.iter().any(|o| o.contains("is_fork=true,idx=0")));
        assert!(outputs.iter().any(|o| o.contains("is_fork=true,idx=1")));
    }

    // ── Tracker: get_children ───────────────────────────────────────────────

    #[tokio::test]
    async fn tracker_get_children_returns_child_run_ids() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "child-1".into(),
                parent_run_id: "parent-X".into(),
                delegation_id: "del-1".into(),
                agent_id: "coder".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "child-2".into(),
                parent_run_id: "parent-X".into(),
                delegation_id: "del-1".into(),
                agent_id: "reviewer".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "other-child".into(),
                parent_run_id: "parent-Y".into(),
                delegation_id: "del-2".into(),
                agent_id: "writer".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

        let mut children = tracker.get_children("parent-X").await;
        children.sort();
        assert_eq!(children, vec!["child-1", "child-2"]);

        let children_y = tracker.get_children("parent-Y").await;
        assert_eq!(children_y, vec!["other-child"]);

        let none = tracker.get_children("nonexistent").await;
        assert!(none.is_empty());
    }

    // ── Tracker: individual pause_sub_run / resume_sub_run ──────────────────

    #[tokio::test]
    async fn pause_and_resume_individual_sub_run() {
        let tracker = DelegationTracker::new();
        let flag = tracker.register_pause_flag("run-1").await;

        assert!(!flag.load(Ordering::Relaxed));
        assert!(!tracker.is_paused("run-1").await);

        // Pause individual sub-run
        assert!(tracker.pause_sub_run("run-1").await);
        assert!(flag.load(Ordering::Relaxed));
        assert!(tracker.is_paused("run-1").await);

        // Resume individual sub-run
        assert!(tracker.resume_sub_run("run-1").await);
        assert!(!flag.load(Ordering::Relaxed));
        assert!(!tracker.is_paused("run-1").await);

        // Pause/resume unknown run returns false
        assert!(!tracker.pause_sub_run("unknown").await);
        assert!(!tracker.resume_sub_run("unknown").await);
    }

    // ── Fan-out: all agents fail ────────────────────────────────────────────

    #[tokio::test]
    async fn fan_out_all_agents_fail() {
        let (reg, engine, tracker) = setup();
        let failing = Arc::new(FailingExecutor {
            fail_agents: vec!["coder".into(), "reviewer".into()],
        });
        let de = DelegationEngine::with_executor(reg, engine, tracker, failing);

        let req = fan_out_request(vec!["coder", "reviewer"]);
        let result = de.execute(req, "orch", None).await.unwrap();

        // All results should be failed
        assert_eq!(result.agent_results.len(), 2);
        for r in &result.agent_results {
            assert_eq!(r.status, "failed");
            assert!(r.error.is_some());
        }
    }

    // ── Executor hard error (Err) vs soft fail (Ok with failed status) ──────

    #[tokio::test]
    async fn executor_hard_error_captured_as_failed_result() {
        /// Executor that returns Err (panic-like failure, not just failed status).
        struct HardErrorExecutor;

        #[async_trait]
        impl SubRunExecutor for HardErrorExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
                Err(format!(
                    "executor crashed for {}",
                    config.agent_profile.agent_id
                ))
            }
        }

        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(HardErrorExecutor));

        let req = fan_out_request(vec!["coder"]);
        let result = de.execute(req, "orch", None).await.unwrap();

        // Hard errors should be captured as failed agent results, not propagated
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, "failed");
        assert!(
            result.agent_results[0]
                .error
                .as_ref()
                .unwrap()
                .contains("crashed")
        );
    }

    // ── Sequential: output chaining across stages ───────────────────────────

    #[tokio::test]
    async fn sequential_output_chaining_verified() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(EchoExecutor));

        let req = DelegationRequest {
            delegation_id: "del-seq-chain".into(),
            parent_run_id: "p1".into(),
            task: "chained task".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into(), "reviewer".into(), "writer".into()],
                stop_on_success: false,
            },
            user_id: "u".into(),
            depth: 0,
            context: HashMap::new(),
        };

        let result = de.execute(req, "orch", None).await.unwrap();
        assert_eq!(result.agent_results.len(), 3);

        // Each stage receives previous output
        let out0 = result.agent_results[0].output.as_ref().unwrap();
        assert!(out0.contains("[coder]"), "first stage should run");

        let out1 = result.agent_results[1].output.as_ref().unwrap();
        assert!(
            out1.contains("prev="),
            "second stage should receive prev output"
        );

        let out2 = result.agent_results[2].output.as_ref().unwrap();
        assert!(
            out2.contains("prev="),
            "third stage should receive prev output"
        );
    }

    // ── DefaultQualityGate tests ────────────────────────────────────────

    fn make_result(status: &str, output: Option<&str>) -> AgentResult {
        AgentResult {
            agent_id: "test".into(),
            run_id: "r1".into(),
            status: status.into(),
            output: output.map(|s| s.to_string()),
            error: if status == "failed" {
                Some("err".into())
            } else {
                None
            },
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
        }
    }

    #[tokio::test]
    async fn quality_gate_passes_normal_output() {
        let gate = DefaultQualityGate::default();
        let result = make_result("completed", Some("This is a perfectly valid agent output."));
        assert!(gate.verify(&result, "d1", 1).await.is_pass());
    }

    #[tokio::test]
    async fn quality_gate_skips_failed_result() {
        // Failed results with no output still fail the min_output_len check.
        // This is by design — the gate checks output quality regardless of status.
        let gate = DefaultQualityGate::default();
        let result = make_result("failed", None);
        let v = gate.verify(&result, "d1", 1).await;
        assert!(!v.is_pass()); // No output → too short
    }

    #[tokio::test]
    async fn quality_gate_fails_no_output() {
        let gate = DefaultQualityGate::default();
        let result = make_result("completed", None);
        let v = gate.verify(&result, "d1", 1).await;
        assert!(!v.is_pass());
    }

    #[tokio::test]
    async fn quality_gate_fails_too_short() {
        let gate = DefaultQualityGate::default();
        let result = make_result("completed", Some("hi"));
        let v = gate.verify(&result, "d1", 1).await;
        assert!(!v.is_pass());
        if let GateVerdict::Fail { reason, .. } = v {
            assert!(reason.contains("too short"));
        }
    }

    #[tokio::test]
    async fn quality_gate_fails_too_long() {
        let gate = DefaultQualityGate::with_thresholds(QualityThresholds {
            max_output_len: 50,
            ..Default::default()
        });
        let result = make_result("completed", Some(&"x".repeat(100)));
        let v = gate.verify(&result, "d1", 1).await;
        assert!(!v.is_pass());
        if let GateVerdict::Fail { reason, .. } = v {
            assert!(reason.contains("too long"));
        }
    }

    #[tokio::test]
    async fn quality_gate_fails_repetitive_output() {
        let gate = DefaultQualityGate::default();
        // Use non-error lines so repetition check fires (not error_dominated).
        let repetitive = "processing data chunk...\n".repeat(20);
        let result = make_result("completed", Some(&repetitive));
        let v = gate.verify(&result, "d1", 1).await;
        assert!(!v.is_pass());
        if let GateVerdict::Fail { reason, .. } = v {
            assert!(reason.contains("repetition"));
        }
    }

    #[tokio::test]
    async fn quality_gate_custom_thresholds() {
        let gate = DefaultQualityGate::with_thresholds(QualityThresholds {
            min_output_len: 1,
            max_output_len: 1_000_000,
            max_repetition_ratio: 0.95,
            max_retries: 5,
        });
        assert_eq!(gate.max_retries(), 5);
        // Slightly repetitive but under 95% threshold — should pass.
        let mut lines = "same line\n".repeat(8);
        lines.push_str("different line 1\n");
        lines.push_str("different line 2\n");
        let result = make_result("completed", Some(&lines));
        assert!(gate.verify(&result, "d1", 1).await.is_pass());
    }
}
