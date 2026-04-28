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

use std::collections::{HashMap, HashSet};

use crate::turn::agentic_loop_host::RequestConstraints;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tokio::sync::RwLock;

use astra_services::LlmTokenServiceConfig;
use astra_services::coordination::{
    AgentProfile, AgentProfileRegistry, AgentResult, AggregationStrategy, CoordinationPattern,
    DelegationRequest, DelegationResult, aggregate_results,
};

pub use astra_core::SubRunState;
use astra_core::{
    InvalidTransition, STATUS_CANCELLED, STATUS_COMPLETED, STATUS_FAILED, STATUS_PAUSED,
    STATUS_RUNNING, STATUS_VERIFICATION_FAILED,
};

use super::run_engine::RunEngine;
use astra_messaging::router::AgentMailboxRouter;
use astra_prompts::team_prompts;

fn normalize_context_allowlist_entry(entry: &str, key: &str) -> Result<String, String> {
    let normalized = entry.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        Err(format!(
            "context[{key}] must not contain empty or whitespace-only strings"
        ))
    } else {
        Ok(normalized)
    }
}

fn parse_request_allowlist_from_context(
    context: &mut HashMap<String, serde_json::Value>,
    key: &str,
) -> Result<Option<HashSet<String>>, String> {
    let Some(value) = context.remove(key) else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .ok_or_else(|| format!("context[{key}] must be an array of strings"))?;
    let mut normalized = HashSet::with_capacity(values.len());
    for entry in values {
        let raw = entry
            .as_str()
            .ok_or_else(|| format!("context[{key}] must contain only strings"))?;
        normalized.insert(normalize_context_allowlist_entry(raw, key)?);
    }
    Ok(Some(normalized))
}

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
    /// Trusted forwarded headers propagated out-of-band for child remote skills.
    pub forward_headers: HashMap<String, String>,
    /// Optional request-scoped LLM token service for child loop model resolution.
    pub llm_token_service: Option<LlmTokenServiceConfig>,
    /// Request-scoped capability constraints inherited from the parent runtime request.
    pub request_constraints: RequestConstraints,
    /// Current nested agent/sub-run depth for the child loop.
    pub recursion_depth: u8,
    /// Cooperative pause flag — checked between turns by the sub-run loop.
    /// When set to `true`, the sub-run should yield with status "paused".
    pub pause_flag: Option<Arc<AtomicBool>>,
    /// Mid-execution checkpoint gate — abort early if contract criteria are violated.
    pub checkpoint_gate: Option<Arc<dyn CheckpointGate>>,
    /// Optional mailbox for inter-agent messaging during the sub-run.
    pub mailbox: Option<astra_messaging::router::AgentMailbox>,
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
            .field("forward_headers", &!self.forward_headers.is_empty())
            .field("llm_token_service", &self.llm_token_service.is_some())
            .field("request_constraints", &self.request_constraints)
            .field("recursion_depth", &self.recursion_depth)
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

        // Check for binary garbage (null bytes)
        let null_count = output.as_bytes().iter().filter(|&&b| b == 0).count();
        if null_count > 5 || (null_count > 0 && null_count * 100 > output.len()) {
            return GateVerdict::Fail {
                reason: format!(
                    "output contains binary garbage ({null_count} null bytes in {} bytes)",
                    output.len()
                ),
                details: Some(serde_json::json!({
                    "check": "binary_garbage",
                    "null_bytes": null_count,
                    "total_len": output.len(),
                })),
            };
        }

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

// ─── Sub-run Tracking ─────────────────────────────────────────────────────────────

// SubRunRecord and DelegationProgress are now defined in astra-server-types.
pub use astra_server_types::team_orchestrator_traits::{DelegationProgress, SubRunRecord};

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
    /// run_id → cancellation token (shared with the sub-run's loop)
    cancel_tokens: RwLock<HashMap<String, Arc<tokio_util::sync::CancellationToken>>>,
    /// Optional session ID for journal persistence.
    session_id: Option<String>,
    /// Real-time progress per delegation.
    progress: RwLock<HashMap<String, DelegationProgress>>,
    /// Optional progress broadcaster for SSE events.
    progress_broadcaster: Option<Arc<crate::orchestration::ProgressBroadcaster>>,
}

impl DelegationTracker {
    pub fn new() -> Self {
        Self {
            delegations: RwLock::new(HashMap::new()),
            parents: RwLock::new(HashMap::new()),
            pause_flags: RwLock::new(HashMap::new()),
            cancel_tokens: RwLock::new(HashMap::new()),
            session_id: None,
            progress: RwLock::new(HashMap::new()),
            progress_broadcaster: None,
        }
    }

    /// Create a tracker with journal persistence enabled.
    pub fn with_session(session_id: String) -> Self {
        Self {
            delegations: RwLock::new(HashMap::new()),
            parents: RwLock::new(HashMap::new()),
            pause_flags: RwLock::new(HashMap::new()),
            cancel_tokens: RwLock::new(HashMap::new()),
            session_id: Some(session_id),
            progress: RwLock::new(HashMap::new()),
            progress_broadcaster: None,
        }
    }

    /// Attach a progress broadcaster for SSE event emission.
    pub fn with_progress_broadcaster(
        mut self,
        broadcaster: Arc<crate::orchestration::ProgressBroadcaster>,
    ) -> Self {
        self.progress_broadcaster = Some(broadcaster);
        self
    }

    /// Get the progress broadcaster, if configured.
    pub fn progress_broadcaster(&self) -> Option<&Arc<crate::orchestration::ProgressBroadcaster>> {
        self.progress_broadcaster.as_ref()
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
        let mut event = astra_services::session_journal::JournalEvent::base_public(
            event_type,
            Some(sid.as_str()),
        );
        event.metadata = Some(metadata);
        self.persist_journal_entry(event);
    }

    /// Persist a fully constructed journal event (best-effort).
    fn persist_journal_entry(&self, event: astra_services::session_journal::JournalEvent) {
        let Some(ref sid) = self.session_id else {
            return;
        };
        let writer = match astra_services::session_journal::JournalWriter::new(sid) {
            Ok(w) => w,
            Err(e) => {
                astra_core::agent_warn!(
                    "delegation",
                    "JournalWriter::new failed for session {sid}: {e}"
                );
                return;
            }
        };
        if let Err(e) = writer.append(&event) {
            astra_core::agent_warn!("delegation", "Failed to write journal event: {e}");
        }
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
                state: SubRunState::from_str(&rec.status).unwrap_or_else(|| {
                    eprintln!(
                        "[delegation-tracker] unknown status '{}' for run '{}', defaulting to Failed",
                        rec.status, rec.run_id
                    );
                    SubRunState::Failed
                }),
                retry_of: rec.retry_of.clone(),
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
        let agent_id = record.agent_id.clone();

        // Emit SSE event for web clients
        if let Some(ref broadcaster) = self.progress_broadcaster {
            use crate::orchestration::{AgentProgressEvent, ProgressEventType};
            broadcaster.emit(AgentProgressEvent {
                agent_id: agent_id.clone(),
                event_type: ProgressEventType::AgentSpawned {
                    run_id: run_id.clone(),
                    parent_run_id: parent_id.clone(),
                    agent_type: "delegated".to_string(),
                    description: format!("Sub-run for delegation {}", &delegation_id),
                },
                timestamp_epoch_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
            });
        }

        // LOCK ORDER: delegations → parents (matches `cleanup_delegation` and
        // `load_from_run_records`). Both maps must be inserted into atomically
        // so concurrent `is_sub_run` cannot observe the parents map without the
        // matching delegations entry (and vice-versa).
        let mut delegations = self.delegations.write().await;
        let mut parents = self.parents.write().await;
        delegations.entry(delegation_id).or_default().push(record);
        parents.insert(run_id, parent_id);
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

    /// Get the current state of a sub-run by its run_id.
    pub async fn get_sub_run_state(&self, run_id: &str) -> Option<SubRunState> {
        for records in self.delegations.read().await.values() {
            for record in records {
                if record.run_id == run_id {
                    return Some(record.state);
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

    /// Register a cancellation token for a sub-run so `cancel_children_of` can cancel it.
    pub async fn register_cancel_token(
        &self,
        run_id: &str,
        token: Arc<tokio_util::sync::CancellationToken>,
    ) {
        self.cancel_tokens
            .write()
            .await
            .insert(run_id.to_string(), token);
    }

    /// Cancel ALL sub-runs that have a given parent run ID.
    /// Returns the number of sub-runs cancelled.
    pub async fn cancel_children_of(&self, parent_run_id: &str) -> usize {
        let children = self.get_children(parent_run_id).await;
        let tokens = self.cancel_tokens.read().await;
        let flags = self.pause_flags.read().await;
        let mut count = 0;
        for child_id in &children {
            if let Some(token) = tokens.get(child_id) {
                token.cancel();
                count += 1;
            }
            // Also set pause flag to stop cooperative loops
            if let Some(flag) = flags.get(child_id) {
                flag.store(true, Ordering::SeqCst);
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
            .is_some_and(|f| f.load(Ordering::Acquire))
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
                    let parent_run_id = record.parent_run_id.clone();
                    let depth = record.depth;
                    let retry_of = record.retry_of.clone();
                    drop(delegations);

                    if new_state == SubRunState::Running {
                        self.persist_event(
                            astra_services::session_journal::JournalEventType::DelegationSubRunStarted,
                            serde_json::json!({
                                "delegation_id": delegation_id,
                                "sub_run_id": run_id,
                                "parent_run_id": parent_run_id,
                                "agent_id": agent_id,
                                "status": new_state.as_str(),
                                "depth": depth,
                                "retry_of": retry_of,
                            }),
                        );
                    }

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
        self.complete_sub_run_with_result(run_id, terminal_state, None, None)
            .await;
    }

    /// Mark a sub-run as complete and persist the terminal result metadata.
    pub async fn complete_sub_run_with_result(
        &self,
        run_id: &str,
        terminal_state: SubRunState,
        error: Option<&str>,
        output_preview: Option<&str>,
    ) {
        debug_assert!(terminal_state.is_terminal());

        // Transition state in record
        let mut delegation_id = None;
        let mut agent_id = None;
        let mut final_state = terminal_state;
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
                        final_state = record.state;
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

        // Update progress + emit SSE event
        if let (Some(did), Some(aid)) = (delegation_id, agent_id) {
            self.persist_journal_entry(
                astra_services::session_journal::JournalEvent::delegation_sub_run_completed(
                    self.session_id.as_deref(),
                    &did,
                    run_id,
                    &aid,
                    final_state.as_str(),
                    error,
                    output_preview,
                ),
            );

            self.update_progress(&did, &aid, final_state).await;

            // Emit completion SSE event for web clients
            if let Some(ref broadcaster) = self.progress_broadcaster {
                use crate::orchestration::{AgentProgressEvent, ProgressEventType};
                let status_str = format!("{:?}", final_state);
                let event_type = if final_state == SubRunState::Completed {
                    ProgressEventType::Completed {
                        result_summary: format!("Sub-run {} finished", run_id),
                        total_tool_calls: 0,
                        total_tokens: (0, 0),
                        duration_ms: 0,
                    }
                } else {
                    ProgressEventType::Failed {
                        error: format!("Sub-run terminal state: {}", status_str),
                    }
                };
                broadcaster.emit(AgentProgressEvent {
                    agent_id: aid,
                    event_type,
                    timestamp_epoch_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64,
                });
            }
        }
    }

    /// Bulk cleanup after a full delegation completes.
    ///
    /// Cleans up all tracking state for a completed delegation:
    /// progress entries, pause flags, parent mappings, and delegation records.
    /// Call after the delegation lifecycle is fully complete.
    pub async fn cleanup_delegation(&self, delegation_id: &str) -> Result<(), String> {
        // Gather run_ids before cleanup
        let records = self.get_sub_runs(delegation_id).await;
        let non_terminal: Vec<String> = records
            .iter()
            .filter(|record| !record.state.is_terminal())
            .map(|record| format!("{}({})", record.run_id, record.state.as_str()))
            .collect();
        if !non_terminal.is_empty() {
            return Err(format!(
                "delegation {delegation_id} still has non-terminal sub-runs: {}",
                non_terminal.join(", ")
            ));
        }
        let run_ids: Vec<String> = records.iter().map(|r| r.run_id.clone()).collect();

        // Acquire locks in consistent order: delegations → parents → pause_flags → cancel_tokens → progress
        // (same order as load_from_run_records to prevent deadlock)
        let mut delegations = self.delegations.write().await;
        let mut parents = self.parents.write().await;
        let mut pause_flags = self.pause_flags.write().await;
        let mut cancel_tokens = self.cancel_tokens.write().await;
        let mut progress_map = self.progress.write().await;

        delegations.remove(delegation_id);
        for rid in &run_ids {
            parents.remove(rid);
            pause_flags.remove(rid);
            cancel_tokens.remove(rid);
        }
        progress_map.remove(delegation_id);
        Ok(())
    }

    /// Get the full retry chain for a run: [original, retry1, retry2, ...]
    pub async fn get_retry_chain(&self, run_id: &str) -> Vec<String> {
        let delegations = self.delegations.read().await;

        // Find which delegation this run belongs to
        for records in delegations.values() {
            // First find the original (walk backward via retry_of)
            let mut original_id = run_id.to_string();
            let mut visited = std::collections::HashSet::new();
            loop {
                if !visited.insert(original_id.clone()) {
                    break; // Cycle detected
                }
                let found = records
                    .iter()
                    .find(|r| r.run_id == original_id && r.retry_of.is_some());
                match found {
                    Some(r) => {
                        original_id = r.retry_of.clone().unwrap_or_else(|| {
                            unreachable!("guarded by retry_of.is_some() check above")
                        })
                    }
                    None => break,
                }
            }

            // Now collect forward: original → retries
            let mut chain = vec![original_id.clone()];
            let mut current = original_id;
            visited.clear();
            loop {
                if !visited.insert(current.clone()) {
                    break; // Cycle detected
                }
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

#[async_trait]
impl astra_messaging::DelegationLookup for DelegationTracker {
    async fn get_parent(&self, run_id: &str) -> Option<String> {
        self.get_parent(run_id).await
    }
    async fn get_agent_id(&self, run_id: &str) -> Option<String> {
        self.get_agent_id(run_id).await
    }
    async fn get_depth(&self, run_id: &str) -> Option<u32> {
        self.get_depth(run_id).await
    }
    async fn record_sub_run(&self, info: astra_messaging::SubRunInfo) {
        self.record_sub_run(SubRunRecord {
            run_id: info.run_id,
            parent_run_id: info.parent_run_id,
            delegation_id: info.delegation_id,
            agent_id: info.agent_id,
            depth: info.depth,
            state: SubRunState::Created,
            retry_of: None,
        })
        .await;
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

    /// Get the progress broadcaster from the underlying tracker, if configured.
    pub fn progress_broadcaster(&self) -> Option<&Arc<crate::orchestration::ProgressBroadcaster>> {
        self.tracker.progress_broadcaster()
    }

    /// Dynamically set the verification gate (e.g., per-subtask criteria during plan execution).
    ///
    /// Unlike [`with_gate`] (builder pattern), this mutates the engine in place so callers
    /// can swap gates between delegation calls without rebuilding the engine.
    pub fn set_gate(&mut self, gate: Arc<dyn VerificationGate>) {
        self.gate = Some(gate);
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
        parent_run_id: &str,
        retry_timeout: Option<std::time::Duration>,
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
                        let verification_error =
                            format!("verification gate failed after {attempt} attempts: {reason}");
                        self.tracker
                            .complete_sub_run_with_result(
                                &current.run_id,
                                SubRunState::VerificationFailed,
                                Some(verification_error.as_str()),
                                current.output.as_deref(),
                            )
                            .await;
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
                            error: Some(verification_error),
                            ..current
                        };
                    }

                    // Retry: re-execute with the same config
                    attempt += 1;
                    let original_run_id = current.run_id.clone();
                    let mut retry_config = config_builder();
                    let retry_run_id = retry_config.run_id.clone();
                    let retry_depth = self.tracker.get_depth(&original_run_id).await.unwrap_or(0);

                    astra_core::log_persist!(
                        self.run_engine
                            .start_run_ext(
                                &retry_run_id,
                                &retry_config.user_id,
                                &retry_config.session_id,
                                Some(parent_run_id),
                                Some(delegation_id),
                                Some(&retry_config.agent_profile.agent_id),
                                Some(&original_run_id),
                            )
                            .await,
                        "delegation",
                        &retry_run_id,
                        "start_retry_run"
                    );

                    // Record retry sub-run with linkage to original
                    self.tracker
                        .record_sub_run(SubRunRecord {
                            run_id: retry_run_id.clone(),
                            parent_run_id: parent_run_id.to_string(),
                            delegation_id: delegation_id.to_string(),
                            agent_id: retry_config.agent_profile.agent_id.clone(),
                            depth: retry_depth,
                            state: SubRunState::Created,
                            retry_of: Some(original_run_id.clone()),
                        })
                        .await;

                    let retry_pause_flag = self.tracker.register_pause_flag(&retry_run_id).await;
                    retry_config.pause_flag = Some(retry_pause_flag);
                    let retry_cancel_token = retry_config
                        .cancel_token
                        .as_ref()
                        .map(|t| Arc::new(t.child_token()))
                        .unwrap_or_else(|| Arc::new(tokio_util::sync::CancellationToken::new()));
                    self.tracker
                        .register_cancel_token(&retry_run_id, retry_cancel_token.clone())
                        .await;
                    retry_config.cancel_token = Some(retry_cancel_token);

                    if retry_config.mailbox.is_none() {
                        if let Some(router) = &self.mailbox_router {
                            let addr = astra_messaging::types::AgentAddress {
                                run_id: retry_run_id.clone(),
                                agent_id: retry_config.agent_profile.agent_id.clone(),
                            };
                            match router.register(addr, Some(delegation_id.to_string())).await {
                                Ok(mailbox) => retry_config.mailbox = Some(mailbox),
                                Err(e) => {
                                    eprintln!(
                                        "  ⚠ delegation: mailbox registration failed for retry {}: {}",
                                        retry_config.agent_profile.agent_id, e
                                    );
                                }
                            }
                        }
                    }

                    Self::write_journal_event(
                        &retry_config.session_id,
                        astra_services::session_journal::JournalEvent::delegation_retry(
                            Some(&retry_config.session_id),
                            delegation_id,
                            &original_run_id,
                            &retry_run_id,
                            &retry_config.agent_profile.agent_id,
                            attempt,
                            &reason,
                        ),
                    );

                    // Mark the original as verification-failed before retrying
                    self.tracker
                        .complete_sub_run_with_result(
                            &original_run_id,
                            SubRunState::VerificationFailed,
                            Some(reason.as_str()),
                            current.output.as_deref(),
                        )
                        .await;

                    // Transition retry to Running before execution
                    if let Err(e) = self
                        .tracker
                        .transition_state(&retry_run_id, SubRunState::Running)
                        .await
                    {
                        astra_core::agent_warn!(
                            "delegation",
                            "Retry transition to Running failed for {retry_run_id}: {e:?}"
                        );
                    }

                    let retry_cancel = retry_config.cancel_token.clone();
                    let retry_agent_id = retry_config.agent_profile.agent_id.clone();
                    let retry_exec = async {
                        match retry_timeout {
                            Some(dur) => {
                                match tokio::time::timeout(dur, self.executor.execute(retry_config))
                                    .await
                                {
                                    Ok(r) => r,
                                    Err(_) => Err(format!(
                                        "agent {} exceeded retry timeout of {}s",
                                        retry_agent_id,
                                        dur.as_secs()
                                    )),
                                }
                            }
                            None => self.executor.execute(retry_config).await,
                        }
                    };

                    match if let Some(token) = retry_cancel {
                        tokio::select! {
                            r = retry_exec => r,
                            _ = token.cancelled() => Err("cancelled by budget timeout".to_string()),
                        }
                    } else {
                        retry_exec.await
                    } {
                        Ok(r) => {
                            // Transition retry to Running→Completed/Failed
                            let terminal_state = if r.is_success() {
                                SubRunState::Completed
                            } else {
                                SubRunState::Failed
                            };
                            self.tracker
                                .complete_sub_run_with_result(
                                    &r.run_id,
                                    terminal_state,
                                    r.error.as_deref(),
                                    r.output.as_deref(),
                                )
                                .await;
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
                            self.tracker
                                .complete_sub_run_with_result(
                                    &retry_run_id,
                                    SubRunState::Failed,
                                    Some(e.as_str()),
                                    None,
                                )
                                .await;
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
        self.execute_with_forward_headers(
            request,
            source_agent_id,
            cancel_token,
            HashMap::new(),
            None,
        )
        .await
    }

    pub async fn execute_with_forward_headers(
        &self,
        mut request: DelegationRequest,
        source_agent_id: &str,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
        forward_headers: HashMap<String, String>,
        llm_token_service: Option<LlmTokenServiceConfig>,
    ) -> Result<DelegationResult, String> {
        request
            .context
            .remove(crate::turn::agentic_delegate_interception::FORWARD_HEADERS_CONTEXT_KEY);
        let request_constraints = RequestConstraints::new(
            parse_request_allowlist_from_context(
                &mut request.context,
                crate::turn::agentic_delegate_interception::REQUEST_ALLOWED_TOOLS_CONTEXT_KEY,
            )?,
            parse_request_allowlist_from_context(
                &mut request.context,
                crate::turn::agentic_delegate_interception::REQUEST_ALLOWED_SKILLS_CONTEXT_KEY,
            )?,
        );

        // Validate first
        self.validate(&request, source_agent_id).await?;
        let child_recursion_depth =
            astra_turn_core::agentic_recursion_guard::checked_child_recursion_depth_u32(
                request.depth,
            )?;

        let session_id = request
            .context
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("delegation");

        // Extract pattern name and agent_ids for journal event.
        let (pattern_name, agent_ids_for_journal): (&str, Vec<String>) = match &request.pattern {
            CoordinationPattern::FanOut { agent_ids, .. } => ("fan_out", agent_ids.clone()),
            CoordinationPattern::Pipeline { stages, .. } => (
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

        // Register the parent/orchestrator with the mailbox router so child
        // agents can send progress and messages to `MessageTarget::Parent`.
        // Without this, `resolve_parent_addr` falls back to a synthetic address
        // that has no inbox in the transport, causing `AgentNotFound` errors.
        //
        // Uses `register_if_absent` to atomically skip if the caller already
        // registered this run_id (e.g., CLI layer or tests that pre-register
        // a parent mailbox to receive messages).
        let parent_mailbox = if let Some(router) = &self.mailbox_router {
            let parent_addr = astra_messaging::types::AgentAddress {
                run_id: request.parent_run_id.clone(),
                agent_id: source_agent_id.to_string(),
            };
            match router.register_if_absent(parent_addr, None).await {
                Ok(mb) => mb, // Some(mailbox) if newly registered, None if already present
                Err(e) => {
                    tracing::warn!(
                        target: "astra_runtime::delegation",
                        parent_run_id = %request.parent_run_id,
                        error = %e,
                        "failed to register parent mailbox; child progress messages will be lost",
                    );
                    None
                }
            }
        } else {
            None
        };

        // Note: parent_mailbox cleanup on panic is handled by AgentMailbox's
        // Drop impl, which spawns a background unregister task. On the normal
        // path, we unregister explicitly below for proper error handling.

        let result = match &request.pattern {
            CoordinationPattern::FanOut {
                agent_ids,
                aggregation,
                timeout_sec,
            } => {
                self.execute_fan_out(
                    &request,
                    agent_ids,
                    aggregation,
                    &forward_headers,
                    llm_token_service.as_ref(),
                    &request_constraints,
                    child_recursion_depth,
                    *timeout_sec,
                    cancel_token.as_ref(),
                )
                .await
            }
            CoordinationPattern::Pipeline {
                stages,
                timeout_sec,
            } => {
                let agent_ids: Vec<String> = stages.iter().map(|s| s.agent_id.clone()).collect();
                self.execute_sequential(
                    &request,
                    &agent_ids,
                    false,
                    &forward_headers,
                    llm_token_service.as_ref(),
                    &request_constraints,
                    child_recursion_depth,
                    *timeout_sec,
                    cancel_token.as_ref(),
                )
                .await
            }
            CoordinationPattern::Sequential {
                agent_ids,
                stop_on_success,
                timeout_sec,
            } => {
                self.execute_sequential(
                    &request,
                    agent_ids,
                    *stop_on_success,
                    &forward_headers,
                    llm_token_service.as_ref(),
                    &request_constraints,
                    child_recursion_depth,
                    *timeout_sec,
                    cancel_token.as_ref(),
                )
                .await
            }
            CoordinationPattern::AdversarialReview {
                producer_id,
                reviewer_id,
                max_rounds,
                timeout_sec,
                ..
            } => {
                self.execute_adversarial(
                    &request,
                    producer_id,
                    reviewer_id,
                    *max_rounds,
                    &forward_headers,
                    llm_token_service.as_ref(),
                    &request_constraints,
                    child_recursion_depth,
                    *timeout_sec,
                    cancel_token.as_ref(),
                )
                .await
            }
            CoordinationPattern::Fork {
                tasks,
                agent_id,
                max_turns,
                aggregation,
                timeout_sec,
            } => {
                self.execute_fork(
                    &request,
                    tasks,
                    agent_id,
                    *max_turns,
                    aggregation,
                    &forward_headers,
                    llm_token_service.as_ref(),
                    &request_constraints,
                    child_recursion_depth,
                    *timeout_sec,
                    cancel_token.as_ref(),
                )
                .await
            }
        };

        // Unregister the parent mailbox now that all children have completed.
        // This prevents resource leaks and address collisions with future runs.
        if let (Some(router), Some(mb)) = (&self.mailbox_router, &parent_mailbox) {
            let addr = mb.address.clone();
            if let Err(e) = router.unregister(&addr).await {
                tracing::warn!(
                    target: "astra_runtime::delegation",
                    parent_run_id = %addr.run_id,
                    error = %e,
                    "failed to unregister parent mailbox after delegation",
                );
            }
        }
        // Drop parent_mailbox explicitly before journal write so the Drop
        // impl doesn't race with the explicit unregister above.
        drop(parent_mailbox);

        // Journal: delegation completed
        if let Ok(ref dr) = result {
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
                    dr.aggregated_output.as_deref(),
                ),
            );
        }

        // Note: cleanup_delegation() is intentionally NOT called here.
        // The caller (e.g., TeamExecutionOrchestrator) should call
        // tracker.cleanup_delegation() when the delegation lifecycle is
        // fully complete, including any post-execution inspection.

        result
    }

    /// Write a journal event (best-effort, non-blocking).
    fn write_journal_event(session_id: &str, event: astra_services::session_journal::JournalEvent) {
        if let Ok(writer) = astra_services::session_journal::JournalWriter::new(session_id) {
            if let Err(e) = writer.append(&event) {
                astra_core::agent_warn!("delegation", "Failed to write journal event: {e}");
            }
        }
    }

    /// Fan-out: spawn all agents in parallel, aggregate results.
    async fn execute_fan_out(
        &self,
        request: &DelegationRequest,
        agent_ids: &[String],
        aggregation: &AggregationStrategy,
        forward_headers: &HashMap<String, String>,
        llm_token_service: Option<&LlmTokenServiceConfig>,
        request_constraints: &RequestConstraints,
        child_recursion_depth: u8,
        timeout_sec: u64,
        cancel_token: Option<&Arc<tokio_util::sync::CancellationToken>>,
    ) -> Result<DelegationResult, String> {
        let reg = self.registry.read().await;
        let has_gate = self.gate.is_some();

        // Compute aggregation strategy name and budget info for team prompts
        let aggregation_name = match aggregation {
            AggregationStrategy::FirstSuccess => "FirstSuccess",
            AggregationStrategy::AllResults => "AllResults",
            AggregationStrategy::Consensus => "Consensus",
            AggregationStrategy::LlmGuided { .. } => "LlmGuided",
        };
        let budget_prompt = Self::extract_budget_prompt(&request.context);
        let agent_id_strs: Vec<&str> = agent_ids.iter().map(|s| s.as_str()).collect();

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
                    None,
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
            if let Err(e) = self
                .tracker
                .transition_state(&sub_run_id, SubRunState::Running)
                .await
            {
                astra_core::agent_warn!(
                    "delegation",
                    "Fan-out: transition to Running failed for {sub_run_id}: {e:?}"
                );
            }

            self.run_engine
                .persist_status(&sub_run_id, STATUS_RUNNING, Some("agent_execution"), None)
                .await?;

            let pause_flag = self.tracker.register_pause_flag(&sub_run_id).await;
            // Create a per-child cancel token derived from the parent's token.
            // Cancelling the parent automatically cancels all children.
            let child_cancel = cancel_token
                .map(|t| Arc::new(t.child_token()))
                .unwrap_or_else(|| Arc::new(tokio_util::sync::CancellationToken::new()));
            self.tracker
                .register_cancel_token(&sub_run_id, child_cancel.clone())
                .await;

            let profile = reg.get(agent_id).cloned().unwrap_or_else(|| {
                AgentProfile::new(
                    agent_id,
                    agent_id,
                    astra_services::coordination::AgentTier::User,
                )
            });

            // Register with mailbox router and obtain a mailbox handle (if router available).
            let mailbox = if let Some(router) = &self.mailbox_router {
                let addr = astra_messaging::types::AgentAddress {
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

            // Inject team coordination prompt into task
            let coordination_prompt = format!(
                "{}{}",
                team_prompts::fan_out_agent_prompt(
                    agent_id,
                    &agent_id_strs,
                    aggregation_name,
                    has_gate,
                ),
                budget_prompt,
            );
            let enhanced_task =
                team_prompts::wrap_task_with_coordination(&coordination_prompt, &request.task);

            configs.push(SubRunConfig {
                run_id: sub_run_id,
                agent_profile: profile,
                task: enhanced_task,
                session_id: request
                    .context
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("delegation")
                    .to_string(),
                user_id: request.user_id.clone(),
                previous_output: None,
                context: request.context.clone(),
                forward_headers: forward_headers.clone(),
                llm_token_service: llm_token_service.cloned(),
                request_constraints: request_constraints.clone(),
                recursion_depth: child_recursion_depth,
                pause_flag: Some(pause_flag),
                checkpoint_gate: None,
                mailbox,
                cancel_token: Some(child_cancel),
            });
        }
        drop(reg);

        // Execute sub-runs in parallel, respecting optional max_parallel limit.
        const MAX_FAN_OUT_AGENTS: usize = 32;
        if configs.len() > MAX_FAN_OUT_AGENTS {
            return Err(format!(
                "Fan-out request with {} agents exceeds limit of {MAX_FAN_OUT_AGENTS}",
                configs.len()
            ));
        }
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

        // Store config templates for fan-out gate retry support.
        // Maps agent_id → (AgentProfile, task, session_id, user_id, context)
        let mut retry_templates: HashMap<
            String,
            (
                AgentProfile,
                String,
                String,
                String,
                HashMap<String, serde_json::Value>,
            ),
        > = HashMap::new();
        for config in &configs {
            retry_templates.insert(
                config.agent_profile.agent_id.clone(),
                (
                    config.agent_profile.clone(),
                    config.task.clone(),
                    config.session_id.clone(),
                    config.user_id.clone(),
                    config.context.clone(),
                ),
            );
        }

        let per_agent_timeout = if timeout_sec > 0 {
            Some(std::time::Duration::from_secs(timeout_sec))
        } else {
            None
        };

        // Use JoinSet for abort-on-drop semantics: if caller times out before
        // collecting all results, remaining tasks are aborted automatically.
        let mut join_set: tokio::task::JoinSet<(Result<AgentResult, String>, String, String)> =
            tokio::task::JoinSet::new();
        // Track agent_id/run_id for panic recovery (JoinSet doesn't preserve spawn order)
        let mut id_map: HashMap<tokio::task::Id, (String, String)> = HashMap::new();

        for config in configs {
            let executor = self.executor.clone();
            let run_engine = self.run_engine.clone();
            let tracker = self.tracker.clone();
            let sem = semaphore.clone();
            let cancel = cancel_token.cloned();
            let agent_timeout = per_agent_timeout;
            // Capture identity before moving config into the closure (panic context)
            let captured_agent_id = config.agent_profile.agent_id.clone();
            let captured_run_id = config.run_id.clone();
            let abort_handle = join_set.spawn(async move {
                // audit-#5: do not panic if the semaphore was closed during shutdown.
                let _permit = match sem {
                    Some(ref s) => match s.acquire().await {
                        Ok(p) => Some(p),
                        Err(_) => {
                            tracing::info!(
                                target: "astra_runtime::delegation",
                                "semaphore closed during shutdown; proceeding without permit"
                            );
                            None
                        }
                    },
                    None => None,
                };
                let run_id = config.run_id.clone();
                let agent_id = config.agent_profile.agent_id.clone();

                // Layer timeouts: per-agent timeout wraps execution,
                // cancellation token wraps that.
                let exec_future = async {
                    match agent_timeout {
                        Some(dur) => {
                            match tokio::time::timeout(dur, executor.execute(config)).await {
                                Ok(r) => r,
                                Err(_) => Err(format!(
                                    "agent execution exceeded per-agent timeout of {}s",
                                    dur.as_secs()
                                )),
                            }
                        }
                        None => executor.execute(config).await,
                    }
                };

                let result = if let Some(ref token) = cancel {
                    tokio::select! {
                        r = exec_future => r,
                        _ = token.cancelled() => Err("cancelled by budget timeout".to_string()),
                    }
                } else {
                    exec_future.await
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
                let (error, output_preview) = match &result {
                    Ok(r) => (r.error.as_deref(), r.output.as_deref()),
                    Err(e) => (Some(e.as_str()), None),
                };
                tracker
                    .complete_sub_run_with_result(&run_id, final_state, error, output_preview)
                    .await;
                (result, agent_id, run_id)
            });
            id_map.insert(abort_handle.id(), (captured_agent_id, captured_run_id));
        }

        let mut results = Vec::new();
        // Cancellation-aware collection: if the cancel token fires while we're
        // waiting for results, abort all remaining tasks and drain what's left.
        // Without this, the loop blocks until all tasks complete even after cancel.
        let mut cancelled = false;
        while let Some(join_result) = {
            if cancelled {
                // After abort_all, drain remaining results without waiting.
                join_set.join_next().await
            } else if let Some(token) = cancel_token {
                tokio::select! {
                    biased;
                    r = join_set.join_next() => r,
                    _ = token.cancelled() => {
                        cancelled = true;
                        join_set.abort_all();
                        join_set.join_next().await
                    }
                }
            } else {
                join_set.join_next().await
            }
        } {
            match join_result {
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
                    // JoinError (panic) — look up identity from id_map using task ID
                    let (panic_agent_id, panic_run_id) = id_map
                        .get(&e.id())
                        .cloned()
                        .unwrap_or_else(|| ("unknown".to_string(), "unknown".to_string()));
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
                    let panic_error = format!("task join error (panic): {e}");
                    self.tracker
                        .complete_sub_run_with_result(
                            &panic_run_id,
                            SubRunState::Failed,
                            Some(panic_error.as_str()),
                            None,
                        )
                        .await;
                    results.push(AgentResult {
                        agent_id: panic_agent_id,
                        run_id: panic_run_id,
                        status: STATUS_FAILED.to_string(),
                        output: None,
                        error: Some(panic_error),
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
                let cancel_for_retry = cancel_token.cloned();
                // Build retry config from stored template
                let template = retry_templates.get(&result.agent_id).cloned();
                let gated = self
                    .apply_gate(
                        result,
                        &did,
                        &request.parent_run_id,
                        per_agent_timeout,
                        || {
                            let (profile, task, sess, uid, ctx) =
                                template.clone().unwrap_or_else(|| {
                                    (
                                        AgentProfile::new(
                                            "stub",
                                            "stub",
                                            astra_services::coordination::AgentTier::User,
                                        ),
                                        String::new(),
                                        String::new(),
                                        String::new(),
                                        HashMap::new(),
                                    )
                                });
                            SubRunConfig {
                                run_id: uuid::Uuid::new_v4().to_string(),
                                agent_profile: profile,
                                task,
                                session_id: sess,
                                user_id: uid,
                                previous_output: None,
                                context: ctx,
                                forward_headers: forward_headers.clone(),
                                llm_token_service: llm_token_service.cloned(),
                                request_constraints: request_constraints.clone(),
                                recursion_depth: child_recursion_depth,
                                pause_flag: None,
                                checkpoint_gate: None,
                                mailbox: None,
                                cancel_token: cancel_for_retry.clone(),
                            }
                        },
                    )
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
        forward_headers: &HashMap<String, String>,
        llm_token_service: Option<&LlmTokenServiceConfig>,
        request_constraints: &RequestConstraints,
        child_recursion_depth: u8,
        timeout_sec: u64,
        cancel_token: Option<&Arc<tokio_util::sync::CancellationToken>>,
    ) -> Result<DelegationResult, String> {
        let reg = self.registry.read().await;
        let mut results = Vec::new();
        let mut previous_output: Option<String> = None;
        let has_gate = self.gate.is_some();
        let total_stages = agent_ids.len();
        let budget_prompt = Self::extract_budget_prompt(&request.context);
        let per_stage_timeout = if timeout_sec > 0 {
            Some(std::time::Duration::from_secs(timeout_sec))
        } else {
            None
        };

        for (stage_index, agent_id) in agent_ids.iter().enumerate() {
            // Check cancellation before starting next sequential agent
            if let Some(token) = cancel_token {
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
                    None,
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
            if let Err(e) = self
                .tracker
                .transition_state(&sub_run_id, SubRunState::Running)
                .await
            {
                astra_core::agent_warn!(
                    "delegation",
                    "Sequential: transition to Running failed for {sub_run_id}: {e:?}"
                );
            }

            self.run_engine
                .persist_status(&sub_run_id, STATUS_RUNNING, Some("agent_execution"), None)
                .await?;

            let pause_flag = self.tracker.register_pause_flag(&sub_run_id).await;
            let child_cancel = cancel_token
                .map(|t| Arc::new(t.child_token()))
                .unwrap_or_else(|| Arc::new(tokio_util::sync::CancellationToken::new()));
            self.tracker
                .register_cancel_token(&sub_run_id, child_cancel.clone())
                .await;

            let profile = reg.get(agent_id).cloned().unwrap_or_else(|| {
                AgentProfile::new(
                    agent_id,
                    agent_id,
                    astra_services::coordination::AgentTier::User,
                )
            });

            let mailbox = if let Some(router) = &self.mailbox_router {
                let addr = astra_messaging::types::AgentAddress {
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

            // Inject sequential/pipeline coordination prompt
            let has_prev = previous_output.is_some();
            let coordination_prompt = format!(
                "{}{}",
                team_prompts::sequential_stage_prompt(
                    stage_index,
                    total_stages,
                    agent_id,
                    has_prev,
                    stop_on_success,
                    has_gate,
                ),
                budget_prompt,
            );
            let enhanced_task =
                team_prompts::wrap_task_with_coordination(&coordination_prompt, &request.task);
            let retry_task = enhanced_task.clone();

            let config = SubRunConfig {
                run_id: sub_run_id.clone(),
                agent_profile: profile,
                task: enhanced_task,
                session_id: request
                    .context
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("delegation")
                    .to_string(),
                user_id: request.user_id.clone(),
                previous_output: previous_output.clone(),
                context: request.context.clone(),
                forward_headers: forward_headers.clone(),
                llm_token_service: llm_token_service.cloned(),
                request_constraints: request_constraints.clone(),
                recursion_depth: child_recursion_depth,
                pause_flag: Some(pause_flag),
                checkpoint_gate: None,
                mailbox,
                cancel_token: Some(child_cancel),
            };

            let exec_result = match per_stage_timeout {
                Some(dur) => match tokio::time::timeout(dur, self.executor.execute(config)).await {
                    Ok(r) => r,
                    Err(_) => Err(format!(
                        "agent {} exceeded per-stage timeout of {}s",
                        agent_id,
                        dur.as_secs()
                    )),
                },
                None => self.executor.execute(config).await,
            };

            let result = match exec_result {
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
                        .complete_sub_run_with_result(
                            &sub_run_id,
                            final_state,
                            r.error.as_deref(),
                            r.output.as_deref(),
                        )
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
                        .complete_sub_run_with_result(
                            &sub_run_id,
                            SubRunState::Failed,
                            Some(e.as_str()),
                            None,
                        )
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
                let sess = request
                    .context
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("delegation")
                    .to_string();
                let uid = request.user_id.clone();
                let ctx = request.context.clone();
                let prev = previous_output.clone();
                let cancel_for_retry = cancel_token.cloned();
                let profile_for_retry = reg.get(agent_id).cloned().unwrap_or_else(|| {
                    AgentProfile::new(
                        agent_id,
                        agent_id,
                        astra_services::coordination::AgentTier::User,
                    )
                });
                self.apply_gate(
                    result,
                    &delegation_id,
                    &request.parent_run_id,
                    per_stage_timeout,
                    || SubRunConfig {
                        run_id: uuid::Uuid::new_v4().to_string(),
                        agent_profile: profile_for_retry.clone(),
                        task: retry_task.clone(),
                        session_id: sess.clone(),
                        user_id: uid.clone(),
                        previous_output: prev.clone(),
                        context: ctx.clone(),
                        forward_headers: forward_headers.clone(),
                        llm_token_service: llm_token_service.cloned(),
                        request_constraints: request_constraints.clone(),
                        recursion_depth: child_recursion_depth,
                        pause_flag: None,
                        checkpoint_gate: None,
                        mailbox: None,
                        cancel_token: cancel_for_retry.clone(),
                    },
                )
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
        forward_headers: &HashMap<String, String>,
        llm_token_service: Option<&LlmTokenServiceConfig>,
        request_constraints: &RequestConstraints,
        child_recursion_depth: u8,
        timeout_sec: u64,
        cancel_token: Option<&Arc<tokio_util::sync::CancellationToken>>,
    ) -> Result<DelegationResult, String> {
        let reg = self.registry.read().await;
        let mut results = Vec::new();
        let mut last_producer_output: Option<String> = None;
        let budget_prompt = Self::extract_budget_prompt(&request.context);
        let per_round_timeout = if timeout_sec > 0 {
            Some(std::time::Duration::from_secs(timeout_sec))
        } else {
            None
        };

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
            if let Some(token) = cancel_token {
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
                    None,
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
            if let Err(e) = self
                .tracker
                .transition_state(&prod_run_id, SubRunState::Running)
                .await
            {
                astra_core::agent_warn!(
                    "delegation",
                    "Adversarial: transition to Running failed for producer {prod_run_id}: {e:?}"
                );
            }
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
                let addr = astra_messaging::types::AgentAddress {
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

            // Inject adversarial producer coordination prompt
            let has_feedback = last_producer_output.is_some();
            let has_gate = self.gate.is_some();
            let prod_coordination = format!(
                "{}{}",
                team_prompts::adversarial_producer_prompt(
                    reviewer_id,
                    max_rounds,
                    round,
                    has_feedback,
                    has_gate,
                ),
                budget_prompt,
            );
            let prod_enhanced_task =
                team_prompts::wrap_task_with_coordination(&prod_coordination, &request.task);
            let prod_retry_task = prod_enhanced_task.clone();

            let prod_config = SubRunConfig {
                run_id: prod_run_id.clone(),
                agent_profile: producer_profile.clone(),
                task: prod_enhanced_task,
                session_id: request
                    .context
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("delegation")
                    .to_string(),
                user_id: request.user_id.clone(),
                previous_output: last_producer_output.clone(),
                context: request.context.clone(),
                forward_headers: forward_headers.clone(),
                llm_token_service: llm_token_service.cloned(),
                request_constraints: request_constraints.clone(),
                recursion_depth: child_recursion_depth,
                pause_flag: Some(prod_pause.clone()),
                checkpoint_gate: None,
                mailbox: prod_mailbox,
                cancel_token: cancel_token.cloned(),
            };
            let prod_exec = match per_round_timeout {
                Some(dur) => {
                    match tokio::time::timeout(dur, self.executor.execute(prod_config)).await {
                        Ok(r) => r,
                        Err(_) => Err(format!(
                            "producer {} exceeded per-round timeout of {}s",
                            producer_id,
                            dur.as_secs()
                        )),
                    }
                }
                None => self.executor.execute(prod_config).await,
            };
            let prod_result = match prod_exec {
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
                        .complete_sub_run_with_result(
                            &prod_run_id,
                            final_state,
                            r.error.as_deref(),
                            r.output.as_deref(),
                        )
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
                        .complete_sub_run_with_result(
                            &prod_run_id,
                            SubRunState::Failed,
                            Some(e.as_str()),
                            None,
                        )
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
                let sess = request
                    .context
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("delegation")
                    .to_string();
                let uid = request.user_id.clone();
                let ctx = request.context.clone();
                let prev = last_producer_output.clone();
                let cancel_for_retry = cancel_token.cloned();
                let pp = producer_profile.clone();
                self.apply_gate(
                    prod_result,
                    &did,
                    &request.parent_run_id,
                    per_round_timeout,
                    || SubRunConfig {
                        run_id: uuid::Uuid::new_v4().to_string(),
                        agent_profile: pp.clone(),
                        task: prod_retry_task.clone(),
                        session_id: sess.clone(),
                        user_id: uid.clone(),
                        previous_output: prev.clone(),
                        context: ctx.clone(),
                        forward_headers: forward_headers.clone(),
                        llm_token_service: llm_token_service.cloned(),
                        request_constraints: request_constraints.clone(),
                        recursion_depth: child_recursion_depth,
                        pause_flag: None,
                        checkpoint_gate: None,
                        mailbox: None,
                        cancel_token: cancel_for_retry.clone(),
                    },
                )
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
                    None,
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
            if let Err(e) = self
                .tracker
                .transition_state(&rev_run_id, SubRunState::Running)
                .await
            {
                astra_core::agent_warn!(
                    "delegation",
                    "Adversarial: transition to Running failed for reviewer {rev_run_id}: {e:?}"
                );
            }
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
                let addr = astra_messaging::types::AgentAddress {
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

            // Inject adversarial reviewer coordination prompt
            let rev_coordination =
                team_prompts::adversarial_reviewer_prompt(producer_id, max_rounds, round);
            let rev_enhanced_task =
                team_prompts::wrap_task_with_coordination(&rev_coordination, &request.task);

            let rev_config = SubRunConfig {
                run_id: rev_run_id.clone(),
                agent_profile: reviewer_profile.clone(),
                task: rev_enhanced_task,
                session_id: request
                    .context
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("delegation")
                    .to_string(),
                user_id: request.user_id.clone(),
                previous_output: last_producer_output.clone(),
                context: request.context.clone(),
                forward_headers: forward_headers.clone(),
                llm_token_service: llm_token_service.cloned(),
                request_constraints: request_constraints.clone(),
                recursion_depth: child_recursion_depth,
                pause_flag: Some(rev_pause),
                checkpoint_gate: None,
                mailbox: rev_mailbox,
                cancel_token: cancel_token.cloned(),
            };
            let rev_exec = match per_round_timeout {
                Some(dur) => {
                    match tokio::time::timeout(dur, self.executor.execute(rev_config)).await {
                        Ok(r) => r,
                        Err(_) => Err(format!(
                            "reviewer {} exceeded per-round timeout of {}s",
                            reviewer_id,
                            dur.as_secs()
                        )),
                    }
                }
                None => self.executor.execute(rev_config).await,
            };
            let rev_result = match rev_exec {
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
                        .complete_sub_run_with_result(
                            &rev_run_id,
                            final_state,
                            r.error.as_deref(),
                            r.output.as_deref(),
                        )
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
                        .complete_sub_run_with_result(
                            &rev_run_id,
                            SubRunState::Failed,
                            Some(e.as_str()),
                            None,
                        )
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
        forward_headers: &HashMap<String, String>,
        llm_token_service: Option<&LlmTokenServiceConfig>,
        request_constraints: &RequestConstraints,
        child_recursion_depth: u8,
        timeout_sec: u64,
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
        let mut handles: tokio::task::JoinSet<(Result<AgentResult, String>, String, String)> =
            tokio::task::JoinSet::new();
        let mut fork_id_map: HashMap<tokio::task::Id, (String, String)> = HashMap::new();
        for (i, task) in tasks.iter().enumerate() {
            let run_id = uuid::Uuid::new_v4().to_string();
            self.run_engine
                .start_run_ext(
                    &run_id,
                    &request.user_id,
                    session_id,
                    Some(&request.parent_run_id),
                    Some(&request.delegation_id),
                    Some(agent_id),
                    None,
                )
                .await?;
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
            if let Err(e) = self
                .tracker
                .transition_state(&run_id, SubRunState::Running)
                .await
            {
                astra_core::agent_warn!(
                    "delegation",
                    "Fork: transition to Running failed for {run_id}: {e:?}"
                );
            }
            if let Err(e) = self
                .run_engine
                .persist_status(&run_id, "running", Some("fork"), None)
                .await
            {
                astra_core::agent_warn!(
                    "delegation",
                    "Fork: failed to persist running status for {run_id}: {e}"
                );
            }
            let pause_flag = self.tracker.register_pause_flag(&run_id).await;

            let fork_mailbox = if let Some(router) = &self.mailbox_router {
                let addr = astra_messaging::types::AgentAddress {
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

            let has_parent_ctx = !parent_messages.as_array().map_or(true, |a| a.is_empty());
            let budget_prompt = Self::extract_budget_prompt(&request.context);
            let fork_coordination = format!(
                "{}{}",
                team_prompts::fork_child_prompt(i, tasks.len(), has_parent_ctx),
                budget_prompt,
            );
            let fork_task = team_prompts::wrap_task_with_coordination(&fork_coordination, task);

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
                forward_headers: forward_headers.clone(),
                llm_token_service: llm_token_service.cloned(),
                request_constraints: request_constraints.clone(),
                recursion_depth: child_recursion_depth,
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
            let per_child_timeout = if timeout_sec > 0 {
                Some(std::time::Duration::from_secs(timeout_sec))
            } else {
                None
            };
            // Capture identity before moving config (panic context)
            let captured_agent_id = config.agent_profile.agent_id.clone();
            let captured_run_id = config.run_id.clone();
            let abort_handle = handles.spawn(async move {
                // audit-#5: do not panic if the semaphore was closed during shutdown.
                let _permit = match sem {
                    Some(ref s) => match s.acquire().await {
                        Ok(p) => Some(p),
                        Err(_) => {
                            tracing::info!(
                                target: "astra_runtime::delegation",
                                "semaphore closed during shutdown; proceeding without permit"
                            );
                            None
                        }
                    },
                    None => None,
                };
                let run_id = config.run_id.clone();
                let agent_id = config.agent_profile.agent_id.clone();

                let exec_future = async {
                    match per_child_timeout {
                        Some(dur) => {
                            match tokio::time::timeout(dur, executor.execute(config)).await {
                                Ok(r) => r,
                                Err(_) => Err(format!(
                                    "fork child exceeded per-child timeout of {}s",
                                    dur.as_secs()
                                )),
                            }
                        }
                        None => executor.execute(config).await,
                    }
                };

                let result = if let Some(token) = cancel_for_spawn {
                    tokio::select! {
                        r = exec_future => r,
                        _ = token.cancelled() => {
                            Err("cancelled by budget timeout".to_string())
                        }
                    }
                } else {
                    exec_future.await
                };
                let final_state = match &result {
                    Ok(r) => {
                        if let Err(e) = run_engine
                            .persist_status(&run_id, &r.status, None, r.error.as_deref())
                            .await
                        {
                            astra_core::agent_warn!(
                                "delegation",
                                "Fork: failed to persist final status for {run_id}: {e}"
                            );
                        }
                        SubRunState::from_str(&r.status).unwrap_or(SubRunState::Failed)
                    }
                    Err(e) => {
                        if let Err(pe) = run_engine
                            .persist_status(&run_id, "failed", None, Some(e))
                            .await
                        {
                            astra_core::agent_warn!(
                                "delegation",
                                "Fork: failed to persist error status for {run_id}: {pe}"
                            );
                        }
                        SubRunState::Failed
                    }
                };
                let (error, output_preview) = match &result {
                    Ok(r) => (r.error.as_deref(), r.output.as_deref()),
                    Err(e) => (Some(e.as_str()), None),
                };
                tracker
                    .complete_sub_run_with_result(&run_id, final_state, error, output_preview)
                    .await;
                (result, agent_id, run_id)
            });
            fork_id_map.insert(abort_handle.id(), (captured_agent_id, captured_run_id));
        }

        // Collect all results (cancellation-aware, abort-on-drop via JoinSet)
        let mut results = Vec::with_capacity(tasks.len());
        let mut fork_cancelled = false;
        while let Some(join_result) = {
            if fork_cancelled {
                handles.join_next().await
            } else if let Some(token) = cancel_token {
                tokio::select! {
                    biased;
                    r = handles.join_next() => r,
                    _ = token.cancelled() => {
                        fork_cancelled = true;
                        handles.abort_all();
                        handles.join_next().await
                    }
                }
            } else {
                handles.join_next().await
            }
        } {
            match join_result {
                Ok((Ok(r), _, _)) => results.push(r),
                Ok((Err(e), agent_id, run_id)) => results.push(AgentResult {
                    agent_id,
                    run_id,
                    status: "failed".to_string(),
                    output: None,
                    error: Some(e),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                }),
                Err(e) => {
                    let (panic_agent_id, panic_run_id) = fork_id_map
                        .get(&e.id())
                        .cloned()
                        .unwrap_or_else(|| ("unknown".to_string(), "unknown".to_string()));
                    if let Err(e2) = self
                        .run_engine
                        .persist_status(
                            &panic_run_id,
                            STATUS_FAILED,
                            None,
                            Some(&format!("fork task panicked: {e}")),
                        )
                        .await
                    {
                        astra_core::agent_warn!(
                            "delegation",
                            "Fork: failed to persist panic status for {panic_run_id}: {e2}"
                        );
                    }
                    let panic_error = format!("fork task panicked: {e}");
                    self.tracker
                        .complete_sub_run_with_result(
                            &panic_run_id,
                            SubRunState::Failed,
                            Some(panic_error.as_str()),
                            None,
                        )
                        .await;
                    results.push(AgentResult {
                        agent_id: panic_agent_id,
                        run_id: panic_run_id,
                        status: "failed".to_string(),
                        output: None,
                        error: Some(panic_error),
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
        // Persist pause status only for non-terminal sub-runs
        for record in self.tracker.get_sub_runs(delegation_id).await {
            if record.state.is_terminal() {
                continue;
            }
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
            if record.state.is_terminal() {
                continue;
            }
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
            if self
                .tracker
                .get_sub_run_state(&child_id)
                .await
                .map_or(false, |s| s.is_terminal())
            {
                continue;
            }
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
            if self
                .tracker
                .get_sub_run_state(&child_id)
                .await
                .map_or(false, |s| s.is_terminal())
            {
                continue;
            }
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

    /// Cancel all non-terminal sub-runs spawned by a parent run.
    /// Returns the number of sub-runs whose status was persisted as cancelled.
    pub async fn cancel_children_of(&self, parent_run_id: &str) -> usize {
        self.tracker.cancel_children_of(parent_run_id).await;
        let mut persisted = 0;
        for child_id in self.tracker.get_children(parent_run_id).await {
            // Only persist cancelled status for non-terminal sub-runs to avoid
            // overwriting completed/failed status in the durable store.
            let is_terminal = self
                .tracker
                .get_sub_run_state(&child_id)
                .await
                .map_or(false, |s| s.is_terminal());
            if !is_terminal {
                astra_core::log_persist!(
                    self.run_engine
                        .persist_status(&child_id, STATUS_CANCELLED, Some("parent_cancel"), None)
                        .await,
                    "delegation",
                    &child_id,
                    "cancel"
                );
                persisted += 1;
            }
        }
        persisted
    }

    /// Extract budget awareness prompt from delegation context.
    fn extract_budget_prompt(
        context: &std::collections::HashMap<String, serde_json::Value>,
    ) -> String {
        let budget = context.get("team_budget").and_then(|v| v.as_u64());
        let max_parallel = context.get("team_max_parallel").and_then(|v| v.as_u64());
        // Also check for timeout
        let timeout = context.get("team_timeout_sec").and_then(|v| v.as_u64());
        if budget.is_some() || max_parallel.is_some() || timeout.is_some() {
            format!(
                "\n{}",
                team_prompts::budget_awareness_prompt(budget, timeout)
            )
        } else {
            String::new()
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

// ─── Trait Implementations ────────────────────────────────────────────────────────

use astra_server_types::team_orchestrator_traits::{DelegationExecutor, DelegationTracking};

#[async_trait::async_trait]
impl DelegationExecutor for DelegationEngine {
    async fn execute_delegation(
        &self,
        request: DelegationRequest,
        source_agent_id: &str,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    ) -> Result<DelegationResult, String> {
        self.execute(request, source_agent_id, cancel_token).await
    }

    async fn get_delegation_progress(&self, delegation_id: &str) -> Option<DelegationProgress> {
        self.tracker().get_progress(delegation_id).await
    }
}

#[async_trait::async_trait]
impl DelegationTracking for DelegationTracker {
    async fn get_sub_runs(&self, delegation_id: &str) -> Vec<SubRunRecord> {
        DelegationTracker::get_sub_runs(self, delegation_id).await
    }

    async fn is_run_paused(&self, run_id: &str) -> bool {
        self.is_paused(run_id).await
    }

    async fn pause_delegation(&self, delegation_id: &str) -> usize {
        DelegationTracker::pause_delegation(self, delegation_id).await
    }

    async fn resume_delegation(&self, delegation_id: &str) -> usize {
        DelegationTracker::resume_delegation(self, delegation_id).await
    }

    async fn cleanup_delegation(&self, delegation_id: &str) -> Result<(), String> {
        DelegationTracker::cleanup_delegation(self, delegation_id).await
    }
}

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
                timeout_sec: 0,
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
                timeout_sec: 0,
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
                timeout_sec: 0,
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
                timeout_sec: 0,
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
                timeout_sec: 0,
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
                timeout_sec: 0,
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
                timeout_sec: 0,
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
                timeout_sec: 0,
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
                timeout_sec: 0,
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
    async fn execute_with_forward_headers_passes_sensitive_headers_sideband() {
        struct ForwardHeadersCheckExecutor;

        #[async_trait]
        impl SubRunExecutor for ForwardHeadersCheckExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
                let has_auth = config.forward_headers.contains_key("authorization");
                let has_context_key = config.context.contains_key(
                    crate::turn::agentic_delegate_interception::FORWARD_HEADERS_CONTEXT_KEY,
                );
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some(format!(
                        "auth_present={has_auth};context_key_present={has_context_key}"
                    )),
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
            Arc::new(ForwardHeadersCheckExecutor),
        );

        let req = DelegationRequest {
            delegation_id: "fh-test".into(),
            parent_run_id: "p".into(),
            task: "check headers".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 0,
            context: HashMap::new(),
        };

        let result = de
            .execute_with_forward_headers(
                req,
                "orch",
                None,
                HashMap::from([(
                    "authorization".to_string(),
                    "Bearer trusted-token".to_string(),
                )]),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            result.agent_results[0].output.as_deref(),
            Some("auth_present=true;context_key_present=false")
        );
    }

    #[tokio::test]
    async fn execute_with_forward_headers_passes_llm_token_service_sideband() {
        struct LlmTokenServiceCheckExecutor;

        #[async_trait]
        impl SubRunExecutor for LlmTokenServiceCheckExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
                let encoded = config
                    .llm_token_service
                    .as_ref()
                    .map(|service| format!("{}|{}", service.url, service.timeout_ms.unwrap_or(0)))
                    .unwrap_or_else(|| "none".to_string());
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some(encoded),
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
            Arc::new(LlmTokenServiceCheckExecutor),
        );

        let req = DelegationRequest {
            delegation_id: "llm-token-test".into(),
            parent_run_id: "p".into(),
            task: "check llm token service".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 0,
            context: HashMap::new(),
        };

        let result = de
            .execute_with_forward_headers(
                req,
                "orch",
                None,
                HashMap::new(),
                Some(LlmTokenServiceConfig {
                    url: "http://catalog:8081/api/v1/chat/completions".to_string(),
                    timeout_ms: Some(2500),
                }),
            )
            .await
            .unwrap();

        assert_eq!(
            result.agent_results[0].output.as_deref(),
            Some("http://catalog:8081/api/v1/chat/completions|2500")
        );
    }

    #[tokio::test]
    async fn execute_ignores_serialized_forward_headers_in_request_context() {
        struct ForwardHeadersCheckExecutor;

        #[async_trait]
        impl SubRunExecutor for ForwardHeadersCheckExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
                let has_auth = config.forward_headers.contains_key("authorization");
                let has_context_key = config.context.contains_key(
                    crate::turn::agentic_delegate_interception::FORWARD_HEADERS_CONTEXT_KEY,
                );
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some(format!(
                        "auth_present={has_auth};context_key_present={has_context_key}"
                    )),
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
            Arc::new(ForwardHeadersCheckExecutor),
        );

        let req = DelegationRequest {
            delegation_id: "fh-context-test".into(),
            parent_run_id: "p".into(),
            task: "check serialized headers".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 0,
            context: HashMap::from([(
                crate::turn::agentic_delegate_interception::FORWARD_HEADERS_CONTEXT_KEY.to_string(),
                serde_json::json!({"authorization": "Bearer evil", "x-workspace-id": "ws-001"}),
            )]),
        };

        let result = de.execute(req, "orch", None).await.unwrap();
        assert_eq!(
            result.agent_results[0].output.as_deref(),
            Some("auth_present=false;context_key_present=false")
        );
    }

    #[test]
    fn parse_request_allowlist_from_context_normalizes_and_dedupes() {
        let key = crate::turn::agentic_delegate_interception::REQUEST_ALLOWED_TOOLS_CONTEXT_KEY;
        let mut context = HashMap::from([(
            key.to_string(),
            serde_json::json!([" Bash ", "bash", "READ_FILE"]),
        )]);

        let parsed = parse_request_allowlist_from_context(&mut context, key)
            .expect("allowlist should parse")
            .expect("allowlist should be present");

        let expected = HashSet::from(["bash".to_string(), "read_file".to_string()]);
        assert_eq!(parsed, expected);
        assert!(
            !context.contains_key(key),
            "key should be removed from context"
        );
    }

    #[test]
    fn parse_request_allowlist_from_context_rejects_non_array_value() {
        let key = crate::turn::agentic_delegate_interception::REQUEST_ALLOWED_TOOLS_CONTEXT_KEY;
        let mut context = HashMap::from([(key.to_string(), serde_json::json!("bash"))]);

        let err = parse_request_allowlist_from_context(&mut context, key)
            .expect_err("non-array allowlist should fail");
        assert!(err.contains("must be an array of strings"));
    }

    #[test]
    fn parse_request_allowlist_from_context_rejects_non_string_or_empty_entries() {
        let key = crate::turn::agentic_delegate_interception::REQUEST_ALLOWED_TOOLS_CONTEXT_KEY;
        let mut non_string_context =
            HashMap::from([(key.to_string(), serde_json::json!(["bash", 42]))]);
        let err = parse_request_allowlist_from_context(&mut non_string_context, key)
            .expect_err("non-string entry should fail");
        assert!(err.contains("must contain only strings"));

        let mut empty_context =
            HashMap::from([(key.to_string(), serde_json::json!(["bash", "   "]))]);
        let err = parse_request_allowlist_from_context(&mut empty_context, key)
            .expect_err("empty entry should fail");
        assert!(err.contains("must not contain empty or whitespace-only strings"));
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
            forward_headers: HashMap::new(),
            llm_token_service: None,
            request_constraints: Default::default(),
            recursion_depth: 1,
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
    async fn pause_children_of_sets_flags_but_preserves_terminal_status() {
        let (_, engine, tracker, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = fan_out_request(vec!["coder", "reviewer"]);
        let result = de.execute(req, "orch", None).await.unwrap();
        assert_eq!(result.agent_results.len(), 2);

        // Pause all children of parent-1 (sub-runs are already completed)
        let paused = de.pause_children_of("parent-1").await;
        assert_eq!(paused, 2);

        // Cooperative flags are set (for use if sub-runs were still running)
        for ar in &result.agent_results {
            assert!(tracker.is_paused(&ar.run_id).await);
        }

        // Durable status is NOT overwritten for terminal sub-runs
        for ar in &result.agent_results {
            let run = engine.load_run(&ar.run_id).await.unwrap().unwrap();
            assert_eq!(run.status, "completed");
        }

        // Resume clears flags
        let resumed = de.resume_children_of("parent-1").await;
        assert_eq!(resumed, 2);
        for ar in &result.agent_results {
            assert!(!tracker.is_paused(&ar.run_id).await);
        }
    }

    #[tokio::test]
    async fn pause_delegation_by_id_sets_flags_preserves_terminal_status() {
        let (_, engine, tracker, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = fan_out_request(vec!["coder", "reviewer"]);
        de.execute(req, "orch", None).await.unwrap();

        let paused = de.pause_delegation("del-1").await;
        assert_eq!(paused, 2);

        let subs = tracker.get_sub_runs("del-1").await;
        for sub in &subs {
            assert!(tracker.is_paused(&sub.run_id).await);
            // Durable status preserved — terminal sub-runs not overwritten
            let run = engine.load_run(&sub.run_id).await.unwrap().unwrap();
            assert_eq!(run.status, "completed");
        }

        let resumed = de.resume_delegation("del-1").await;
        assert_eq!(resumed, 2);
        for sub in &subs {
            assert!(!tracker.is_paused(&sub.run_id).await);
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
    async fn default_gate_rejects_binary_garbage() {
        let gate = DefaultQualityGate::new();
        let result = AgentResult {
            agent_id: "test".into(),
            run_id: "r1".into(),
            status: "completed".into(),
            output: Some("some text\0\0\0\0\0\0\0\0\0\0garbage".into()),
            error: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
        };
        let verdict = gate.verify(&result, "d1", 0).await;
        assert!(
            matches!(verdict, GateVerdict::Fail { .. }),
            "should reject output with null bytes"
        );
    }

    #[tokio::test]
    async fn default_gate_passes_clean_output() {
        let gate = DefaultQualityGate::new();
        let result = AgentResult {
            agent_id: "test".into(),
            run_id: "r1".into(),
            status: "completed".into(),
            output: Some("This is a perfectly normal output with enough content.".into()),
            error: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
        };
        let verdict = gate.verify(&result, "d1", 0).await;
        assert!(matches!(verdict, GateVerdict::Pass));
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
                timeout_sec: 0,
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
    async fn gate_retry_preserves_sequential_coordination_prompt() {
        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(EchoExecutor));
        let gate = Arc::new(FailThenPassGate::new(1));
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(EchoExecutor))
            .with_gate(gate);

        let req = DelegationRequest {
            delegation_id: "del-seq-gate-prompt".into(),
            parent_run_id: "parent-1".into(),
            task: "sequential gate test".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "user-1".into(),
            depth: 0,
            context: HashMap::new(),
        };
        let result = de.execute(req, "orch", None).await.unwrap();

        let output = result.agent_results[0].output.as_deref().unwrap_or("");
        assert!(output.contains("## Team Coordination: Pipeline"));
        assert!(output.contains("Quality gate active"));
    }

    #[tokio::test]
    async fn gate_retry_preserves_adversarial_coordination_prompt() {
        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(EchoExecutor));
        let gate = Arc::new(FailThenPassGate::new(1));
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(EchoExecutor))
            .with_gate(gate);

        let req = DelegationRequest {
            delegation_id: "del-adv-gate-prompt".into(),
            parent_run_id: "parent-1".into(),
            task: "adversarial gate test".into(),
            pattern: CoordinationPattern::AdversarialReview {
                producer_id: "coder".into(),
                reviewer_id: "reviewer".into(),
                max_rounds: 1,
                timeout_sec: 0,
                acceptance_threshold: 0.8,
            },
            user_id: "user-1".into(),
            depth: 0,
            context: HashMap::new(),
        };
        let result = de.execute(req, "orch", None).await.unwrap();

        let producer_output = result.agent_results[0].output.as_deref().unwrap_or("");
        assert!(producer_output.contains("## Team Coordination: Adversarial Review (Producer)"));
        assert!(producer_output.contains("Quality gate active"));
    }

    #[tokio::test]
    async fn gate_retry_registers_pause_flag() {
        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(EchoExecutor));
        let gate = Arc::new(FailThenPassGate::new(1));
        let de =
            DelegationEngine::with_executor(reg, engine, tracker.clone(), Arc::new(EchoExecutor))
                .with_gate(gate);

        let result = de
            .execute(fan_out_request(vec!["coder"]), "orch", None)
            .await
            .unwrap();
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, "completed");

        let chain = tracker
            .get_retry_chain(&result.agent_results[0].run_id)
            .await;
        assert_eq!(chain.len(), 2);
        assert!(tracker.get_pause_flag(&chain[0]).await.is_some());
        assert!(tracker.get_pause_flag(&chain[1]).await.is_some());
        assert_eq!(de.pause_delegation("del-1").await, 2);
    }

    #[tokio::test]
    async fn gate_retry_preserves_depth_metadata() {
        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(EchoExecutor));
        let gate = Arc::new(FailThenPassGate::new(1));
        let de =
            DelegationEngine::with_executor(reg, engine, tracker.clone(), Arc::new(EchoExecutor))
                .with_gate(gate);

        let result = de
            .execute(fan_out_request(vec!["coder"]), "orch", None)
            .await
            .unwrap();
        let chain = tracker
            .get_retry_chain(&result.agent_results[0].run_id)
            .await;

        assert_eq!(chain.len(), 2);
        assert_eq!(tracker.get_depth(&chain[0]).await, Some(1));
        assert_eq!(tracker.get_depth(&chain[1]).await, Some(1));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn gate_retry_writes_journal_linkage_event() {
        let sessions_dir = std::env::temp_dir().join(format!(
            "delegation-engine-journal-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(&sessions_dir);

        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(EchoExecutor));
        let gate = Arc::new(FailThenPassGate::new(1));
        let de =
            DelegationEngine::with_executor(reg, engine, tracker.clone(), Arc::new(EchoExecutor))
                .with_gate(gate);

        let mut req = fan_out_request(vec!["coder"]);
        req.delegation_id = "del-journal-retry".into();
        req.parent_run_id = "parent-journal-retry".into();
        req.context.insert(
            "session_id".into(),
            serde_json::Value::String("sess-journal-retry".into()),
        );

        let result = de.execute(req, "orch", None).await.unwrap();
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, "completed");

        let chain = tracker
            .get_retry_chain(&result.agent_results[0].run_id)
            .await;
        assert_eq!(chain.len(), 2);

        let journal_path = sessions_dir.join("sess-journal-retry.jsonl");
        let content = std::fs::read_to_string(&journal_path).unwrap();
        let retry_events: Vec<astra_services::session_journal::JournalEvent> = content
            .lines()
            .map(|line| {
                serde_json::from_str::<astra_services::session_journal::JournalEvent>(line).unwrap()
            })
            .filter(|evt| {
                evt.event_type == astra_services::session_journal::JournalEventType::DelegationRetry
            })
            .collect();

        assert_eq!(retry_events.len(), 1);
        let meta = retry_events[0].metadata.as_ref().unwrap();
        assert_eq!(meta["delegation_id"], "del-journal-retry");
        assert_eq!(meta["original_run_id"], chain[0]);
        assert_eq!(meta["retry_run_id"], chain[1]);
        assert_eq!(meta["agent_id"], "coder");
        assert_eq!(meta["attempt"], 2);
        assert_eq!(meta["reason"], "fail #1");

        let _ = std::fs::remove_file(journal_path);
        let _ = std::fs::remove_dir_all(sessions_dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tracker_running_transition_writes_sub_run_started_event() {
        let sessions_dir = std::env::temp_dir().join(format!(
            "delegation-engine-subrun-start-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(&sessions_dir);
        let tracker = DelegationTracker::with_session("sess-subrun-start".into());

        tracker
            .record_sub_run(SubRunRecord {
                run_id: "run-1".into(),
                parent_run_id: "parent-1".into(),
                delegation_id: "del-1".into(),
                agent_id: "coder".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: Some("run-0".into()),
            })
            .await;
        tracker
            .transition_state("run-1", SubRunState::Running)
            .await
            .unwrap();

        let journal_path = sessions_dir.join("sess-subrun-start.jsonl");
        let content = std::fs::read_to_string(&journal_path).unwrap();
        let started_events: Vec<astra_services::session_journal::JournalEvent> = content
            .lines()
            .map(|line| {
                serde_json::from_str::<astra_services::session_journal::JournalEvent>(line).unwrap()
            })
            .filter(|evt| {
                evt.event_type
                    == astra_services::session_journal::JournalEventType::DelegationSubRunStarted
            })
            .collect();

        assert_eq!(started_events.len(), 1);
        let meta = started_events[0].metadata.as_ref().unwrap();
        assert_eq!(meta["delegation_id"], "del-1");
        assert_eq!(meta["sub_run_id"], "run-1");
        assert_eq!(meta["parent_run_id"], "parent-1");
        assert_eq!(meta["agent_id"], "coder");
        assert_eq!(meta["status"], "running");
        assert_eq!(meta["retry_of"], "run-0");

        let _ = std::fs::remove_file(journal_path);
        let _ = std::fs::remove_dir_all(sessions_dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tracker_complete_sub_run_writes_sub_run_completed_event() {
        let sessions_dir = std::env::temp_dir().join(format!(
            "delegation-engine-subrun-complete-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(&sessions_dir);
        let tracker = DelegationTracker::with_session("sess-subrun-complete".into());

        tracker
            .record_sub_run(SubRunRecord {
                run_id: "run-1".into(),
                parent_run_id: "parent-1".into(),
                delegation_id: "del-1".into(),
                agent_id: "coder".into(),
                depth: 1,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;
        tracker
            .complete_sub_run_with_result(
                "run-1",
                SubRunState::Failed,
                Some("boom"),
                Some("partial output"),
            )
            .await;

        let journal_path = sessions_dir.join("sess-subrun-complete.jsonl");
        let content = std::fs::read_to_string(&journal_path).unwrap();
        let completed_events: Vec<astra_services::session_journal::JournalEvent> = content
            .lines()
            .map(|line| {
                serde_json::from_str::<astra_services::session_journal::JournalEvent>(line).unwrap()
            })
            .filter(|evt| {
                evt.event_type
                    == astra_services::session_journal::JournalEventType::DelegationSubRunCompleted
            })
            .collect();

        assert_eq!(completed_events.len(), 1);
        let meta = completed_events[0].metadata.as_ref().unwrap();
        assert_eq!(meta["delegation_id"], "del-1");
        assert_eq!(meta["sub_run_id"], "run-1");
        assert_eq!(meta["agent_id"], "coder");
        assert_eq!(meta["status"], "failed");
        assert_eq!(meta["error"], "boom");
        assert_eq!(meta["output_preview"], "partial output");

        let _ = std::fs::remove_file(journal_path);
        let _ = std::fs::remove_dir_all(sessions_dir);
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
                timeout_sec: 0,
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
                None,
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
                None,
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
                None,
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
                None,
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
                retry_of: None,
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
                retry_of: Some("sub-1".into()),
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
                retry_of: None,
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
        assert_eq!(
            subs.iter()
                .find(|sub| sub.run_id == "sub-2")
                .and_then(|sub| sub.retry_of.as_deref()),
            Some("sub-1")
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
                timeout_sec: 0,
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
                timeout_sec: 0,
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

    // ── State Machine + Lifecycle Tests ──────────────────────────────────

    #[tokio::test]
    async fn tracker_state_transitions() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "r1".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "a1".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

        // Created → Running
        let new = tracker
            .transition_state("r1", SubRunState::Running)
            .await
            .unwrap();
        assert_eq!(new, SubRunState::Running);

        // Running → Completed
        let new = tracker
            .transition_state("r1", SubRunState::Completed)
            .await
            .unwrap();
        assert_eq!(new, SubRunState::Completed);
    }

    #[tokio::test]
    async fn tracker_invalid_transition_rejected() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "r1".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "a1".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

        // Created → Completed should fail (must go through Running)
        let err = tracker.transition_state("r1", SubRunState::Completed).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn tracker_complete_sub_run_updates_state() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "r1".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "a1".into(),
                depth: 1,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;

        tracker.complete_sub_run("r1", SubRunState::Completed).await;

        let subs = tracker.get_sub_runs("d1").await;
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].state, SubRunState::Completed);
    }

    #[tokio::test]
    async fn tracker_retry_chain() {
        let tracker = DelegationTracker::new();
        // Original run
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "r1".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "a1".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;
        // First retry
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "r2".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "a1".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: Some("r1".into()),
            })
            .await;
        // Second retry
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "r3".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "a1".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: Some("r2".into()),
            })
            .await;

        let chain = tracker.get_retry_chain("r3").await;
        assert_eq!(chain, vec!["r1", "r2", "r3"]);

        // Chain from original should return just [r1, r2, r3]
        let chain_from_orig = tracker.get_retry_chain("r1").await;
        assert_eq!(chain_from_orig, vec!["r1", "r2", "r3"]);
    }

    #[tokio::test]
    async fn tracker_cleanup_delegation_removes_all_state() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "r1".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "a1".into(),
                depth: 1,
                state: SubRunState::Completed,
                retry_of: None,
            })
            .await;

        let _f1 = tracker.register_pause_flag("r1").await;
        assert!(tracker.get_pause_flag("r1").await.is_some());
        tracker.init_progress("d1", &["a1".into()]).await;
        assert!(tracker.get_progress("d1").await.is_some());
        assert_eq!(tracker.get_sub_runs("d1").await.len(), 1);

        tracker.cleanup_delegation("d1").await.unwrap();
        assert!(tracker.get_pause_flag("r1").await.is_none());
        assert!(tracker.get_progress("d1").await.is_none());
        assert_eq!(tracker.get_sub_runs("d1").await.len(), 0);
        assert!(tracker.get_children("parent").await.is_empty());
    }

    #[tokio::test]
    async fn tracker_cleanup_delegation_rejects_nonterminal_sub_runs() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "r1".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "a1".into(),
                depth: 1,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;

        let _f1 = tracker.register_pause_flag("r1").await;
        tracker.init_progress("d1", &["a1".into()]).await;

        let err = tracker
            .cleanup_delegation("d1")
            .await
            .expect_err("non-terminal delegation should not be cleaned up");
        assert!(err.contains("r1(running)"), "{err}");
        assert!(tracker.get_pause_flag("r1").await.is_some());
        assert!(tracker.get_progress("d1").await.is_some());
        assert_eq!(tracker.get_sub_runs("d1").await.len(), 1);
        assert_eq!(tracker.get_children("parent").await, vec!["r1".to_string()]);
    }

    #[tokio::test]
    async fn tracker_progress_tracking() {
        let tracker = DelegationTracker::new();
        tracker
            .init_progress("d1", &["a1".into(), "a2".into()])
            .await;

        let progress = tracker.get_progress("d1").await.unwrap();
        assert_eq!(progress.total_count, 2);
        assert_eq!(progress.completed_count, 0);
        assert_eq!(
            *progress.agent_states.get("a1").unwrap(),
            SubRunState::Created
        );

        // Update a1 to Running
        tracker
            .update_progress("d1", "a1", SubRunState::Running)
            .await;
        let progress = tracker.get_progress("d1").await.unwrap();
        assert_eq!(
            *progress.agent_states.get("a1").unwrap(),
            SubRunState::Running
        );
        assert_eq!(progress.completed_count, 0);

        // Complete a1
        tracker
            .update_progress("d1", "a1", SubRunState::Completed)
            .await;
        let progress = tracker.get_progress("d1").await.unwrap();
        assert_eq!(progress.completed_count, 1);
    }

    #[tokio::test]
    async fn cancel_token_per_execution_isolation() {
        let (_, _engine, _tracker, de) = setup_with_executor(Arc::new(EchoExecutor));

        // Create two separate cancel tokens
        let token1 = Arc::new(tokio_util::sync::CancellationToken::new());
        let token2 = Arc::new(tokio_util::sync::CancellationToken::new());

        // Use unique delegation/parent IDs to avoid conflicts
        let mut req1 = fan_out_request(vec!["coder"]);
        req1.delegation_id = "del-iso-1".into();
        req1.parent_run_id = "parent-iso-1".into();

        let mut req2 = fan_out_request(vec!["reviewer"]);
        req2.delegation_id = "del-iso-2".into();
        req2.parent_run_id = "parent-iso-2".into();

        // Execute with different tokens — cancelling one shouldn't affect the other
        let (r1, r2) = tokio::join!(
            de.execute(req1, "orch", Some(token1.clone())),
            de.execute(req2, "orch", Some(token2.clone())),
        );

        // Both should succeed since neither token was cancelled
        assert!(r1.is_ok(), "r1 failed: {:?}", r1.err());
        assert!(r2.is_ok(), "r2 failed: {:?}", r2.err());
    }

    /// Executor that sleeps for a configured duration before returning.
    #[derive(Clone)]
    struct SlowExecutor {
        delay: std::time::Duration,
    }

    #[async_trait::async_trait]
    impl SubRunExecutor for SlowExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            tokio::time::sleep(self.delay).await;
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id.clone(),
                run_id: config.run_id.clone(),
                status: "completed".into(),
                output: Some(format!("slow output for {}", config.task)),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            })
        }
    }

    /// Executor that succeeds immediately on the first call, then sleeps on retry.
    #[derive(Clone)]
    struct RetrySlowExecutor {
        retry_delay: std::time::Duration,
        calls: Arc<std::sync::atomic::AtomicU32>,
    }

    #[async_trait::async_trait]
    impl SubRunExecutor for RetrySlowExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            if call > 0 {
                tokio::time::sleep(self.retry_delay).await;
            }
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id.clone(),
                run_id: config.run_id.clone(),
                status: "completed".into(),
                output: Some(format!("retry-slow output for {}", config.task)),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            })
        }
    }

    /// Executor that reports whether a mailbox was attached to the sub-run config.
    #[derive(Clone)]
    struct MailboxEchoExecutor;

    #[async_trait::async_trait]
    impl SubRunExecutor for MailboxEchoExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id.clone(),
                run_id: config.run_id.clone(),
                status: "completed".into(),
                output: Some(format!("mailbox={}", config.mailbox.is_some())),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            })
        }
    }

    #[tokio::test]
    async fn fan_out_per_agent_timeout_enforced() {
        let slow = Arc::new(SlowExecutor {
            delay: std::time::Duration::from_secs(5),
        });
        let (_, _engine, _tracker, de) = setup_with_executor(slow);

        let req = DelegationRequest {
            delegation_id: "timeout-test".into(),
            parent_run_id: "p".into(),
            task: "slow task".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["coder".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 1, // 1 second timeout, executor sleeps 5s
            },
            user_id: "u".into(),
            depth: 0,
            context: HashMap::new(),
        };

        let result = de.execute(req, "orch", None).await.unwrap();

        // Should fail due to timeout
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, "failed");
        assert!(
            result.agent_results[0]
                .error
                .as_deref()
                .unwrap_or("")
                .contains("timeout"),
            "expected timeout error, got: {:?}",
            result.agent_results[0].error
        );
    }

    #[tokio::test]
    async fn gate_retry_timeout_enforced() {
        let retry_slow = Arc::new(RetrySlowExecutor {
            retry_delay: std::time::Duration::from_secs(5),
            calls: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        });
        let (reg, engine, tracker, _) = setup_with_executor(retry_slow.clone());
        let de = DelegationEngine::with_executor(reg, engine, tracker, retry_slow.clone())
            .with_gate(Arc::new(FailThenPassGate::new(1)));

        let req = DelegationRequest {
            delegation_id: "gate-timeout".into(),
            parent_run_id: "p".into(),
            task: "gated slow retry".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 1,
            },
            user_id: "u".into(),
            depth: 0,
            context: HashMap::new(),
        };

        let result = de.execute(req, "orch", None).await.unwrap();

        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, "failed");
        assert!(
            result.agent_results[0]
                .error
                .as_deref()
                .unwrap_or("")
                .contains("timeout"),
            "expected retry timeout error, got: {:?}",
            result.agent_results[0].error
        );
        assert_eq!(retry_slow.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn gate_retry_registers_mailbox_when_router_present() {
        let (reg, engine, tracker) = setup();
        let gate = Arc::new(FailThenPassGate::new(1));
        let router = Arc::new(crate::messaging::AgentMailboxRouter::new(
            Arc::new(crate::messaging::InProcessTransport::new()),
            tracker.clone(),
        ));
        let de =
            DelegationEngine::with_executor(reg, engine, tracker, Arc::new(MailboxEchoExecutor))
                .with_gate(gate)
                .with_mailbox_router(router);

        let result = de
            .execute(fan_out_request(vec!["coder"]), "orch", None)
            .await
            .unwrap();
        assert_eq!(
            result.agent_results[0].output.as_deref(),
            Some("mailbox=true")
        );
    }

    #[tokio::test]
    async fn sequential_per_stage_timeout_enforced() {
        let slow = Arc::new(SlowExecutor {
            delay: std::time::Duration::from_secs(5),
        });
        let (_, _engine, _tracker, de) = setup_with_executor(slow);

        let req = DelegationRequest {
            delegation_id: "seq-timeout".into(),
            parent_run_id: "p".into(),
            task: "slow pipeline".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into(), "reviewer".into()],
                stop_on_success: false,
                timeout_sec: 1,
            },
            user_id: "u".into(),
            depth: 0,
            context: HashMap::new(),
        };

        let result = de.execute(req, "orch", None).await.unwrap();

        // Both agents should fail due to timeout
        assert_eq!(result.agent_results.len(), 2);
        for ar in &result.agent_results {
            assert_eq!(ar.status, "failed");
            assert!(
                ar.error.as_deref().unwrap_or("").contains("timeout"),
                "expected timeout error for {}, got: {:?}",
                ar.agent_id,
                ar.error
            );
        }
    }

    #[tokio::test]
    async fn zero_timeout_means_no_timeout() {
        let slow = Arc::new(SlowExecutor {
            delay: std::time::Duration::from_millis(50),
        });
        let (_, _engine, _tracker, de) = setup_with_executor(slow);

        let req = DelegationRequest {
            delegation_id: "no-timeout".into(),
            parent_run_id: "p".into(),
            task: "quick task".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["coder".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 0, // no timeout
            },
            user_id: "u".into(),
            depth: 0,
            context: HashMap::new(),
        };

        let result = de.execute(req, "orch", None).await.unwrap();
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, "completed");
    }

    /// audit-#5: closing the semaphore must surface as a graceful Err from
    /// `acquire().await`, not a panic. This is the building-block invariant
    /// that the spawned delegation tasks now rely on (no `.expect`).
    #[tokio::test]
    async fn semaphore_acquire_returns_err_when_closed() {
        use tokio::sync::Semaphore;
        let sem = std::sync::Arc::new(Semaphore::new(0));
        let sem2 = sem.clone();
        let h = tokio::spawn(async move { sem2.acquire().await.map(|_| ()) });
        sem.close();
        let res = h.await.expect("task joins");
        assert!(res.is_err(), "closed semaphore must yield Err, not panic");
    }

    /// audit-#5: source-level guard — no panicking expect calls remain in
    /// the spawned delegation tasks for the closed-semaphore path.
    #[test]
    fn delegation_does_not_panic_on_closed_semaphore() {
        let source = include_str!("delegation_engine.rs");
        // Build the needle dynamically to avoid matching this assertion's
        // own literal in the included source.
        let needle = format!(".expect(\"sem{}closed\")", "aphore ");
        assert_eq!(
            source.matches(needle.as_str()).count(),
            0,
            "spawned delegation tasks must not panic on a closed semaphore"
        );
    }

    /// P1-B: cancel_children_of must cancel all child tokens.
    #[tokio::test]
    async fn cancel_children_of_cancels_tokens() {
        let tracker = DelegationTracker::new();
        let parent = "parent-run";
        let child1 = "child-1";
        let child2 = "child-2";

        // Register children under parent
        tracker
            .record_sub_run(SubRunRecord {
                run_id: child1.into(),
                parent_run_id: parent.into(),
                delegation_id: "deleg-1".into(),
                agent_id: "agent-a".into(),
                depth: 0,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                run_id: child2.into(),
                parent_run_id: parent.into(),
                delegation_id: "deleg-1".into(),
                agent_id: "agent-b".into(),
                depth: 0,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;

        let token1 = Arc::new(tokio_util::sync::CancellationToken::new());
        let token2 = Arc::new(tokio_util::sync::CancellationToken::new());
        tracker.register_cancel_token(child1, token1.clone()).await;
        tracker.register_cancel_token(child2, token2.clone()).await;

        assert!(!token1.is_cancelled());
        assert!(!token2.is_cancelled());

        let count = tracker.cancel_children_of(parent).await;
        assert_eq!(count, 2, "both children must be cancelled");
        assert!(token1.is_cancelled(), "child1 token must be cancelled");
        assert!(token2.is_cancelled(), "child2 token must be cancelled");
    }

    /// P1-B source guard: cancel_run must call cancel_children_of on delegation engine.
    #[test]
    fn cancel_run_cascades_to_delegation_children() {
        let source = include_str!("run_lifecycle.rs");
        let fn_start = source
            .find("async fn cancel_run(")
            .expect("cancel_run must exist");
        let fn_end = source[fn_start..]
            .find("\n    async fn ")
            .map(|p| fn_start + p)
            .unwrap_or(source.len());
        let fn_body = &source[fn_start..fn_end];
        assert!(
            fn_body.contains("cancel_children_of"),
            "cancel_run must cascade cancellation to delegation sub-runs"
        );
    }

    /// cancel_tokens must be cleaned up in cleanup_delegation to prevent memory leaks.
    #[tokio::test]
    async fn cleanup_delegation_removes_cancel_tokens() {
        let tracker = DelegationTracker::new();
        let deleg_id = "deleg-cleanup";
        let child = "child-cleanup";

        tracker
            .record_sub_run(SubRunRecord {
                run_id: child.into(),
                parent_run_id: "parent".into(),
                delegation_id: deleg_id.into(),
                agent_id: "agent".into(),
                depth: 0,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;

        let token = Arc::new(tokio_util::sync::CancellationToken::new());
        tracker.register_cancel_token(child, token.clone()).await;
        assert!(tracker.cancel_tokens.read().await.contains_key(child));

        // Complete the sub-run so cleanup_delegation can proceed
        tracker
            .complete_sub_run(child, SubRunState::Completed)
            .await;

        tracker.cleanup_delegation(deleg_id).await.unwrap();
        assert!(
            !tracker.cancel_tokens.read().await.contains_key(child),
            "cancel_tokens must be cleaned up after delegation cleanup"
        );
    }

    /// Source guard: cleanup_delegation must clean cancel_tokens alongside pause_flags.
    #[test]
    fn cleanup_delegation_cleans_cancel_tokens_source_guard() {
        let source = include_str!("delegation_engine.rs");
        let impl_start = source
            .find("impl DelegationTracker {")
            .expect("impl must exist");
        let impl_source = &source[impl_start..];
        let fn_start = impl_source
            .find("async fn cleanup_delegation(")
            .expect("cleanup_delegation must exist");
        let fn_end = impl_source[fn_start..]
            .find("\n    pub async fn ")
            .or_else(|| impl_source[fn_start..].find("\n    async fn "))
            .map(|p| fn_start + p)
            .unwrap_or(impl_source.len());
        let fn_body = &impl_source[fn_start..fn_end];
        assert!(
            fn_body.contains("cancel_tokens"),
            "cleanup_delegation must clean up cancel_tokens map"
        );
    }
}
