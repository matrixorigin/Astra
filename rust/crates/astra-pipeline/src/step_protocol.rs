//! Step Protocol v1: Slot-based execution, tiered checkpoints, DB-first events.
//!
//! # Architecture: 3 Concerns, 3 Types
//!
//! ```text
//! ┌─ StepDescriptor ──────────────┐  Scheduling layer (who/when/retry)
//! │  step_id, task_id, action,    │  Immutable after creation
//! │  scheduling, retry_policy     │
//! ├─ StepExecution ───────────────┤  Runtime layer (cursor/progress)
//! │  cursor, execution_slots,     │  Mutable during execution
//! │  result, memory_context       │
//! ├─ StepCheckpoint ──────────────┤  Persistence layer (2-tier)
//! │  Light: cursor + metadata     │  Frequent, cheap
//! │  Heavy: + messages + results  │  Infrequent, full recovery
//! └───────────────────────────────┘
//! ```
//!
//! # Key properties
//!
//! - **Versioned**: compound encoding `major*1000+minor`. `VersionPolicy` with negotiation chain.
//! - **Slot-based cursor**: `ExecutionSlot` per tool (state machine), not sequential index.
//! - **Tiered checkpoints**: `LightCheckpoint` (frequent) + `HeavyCheckpoint` (full recovery).
//! - **Checkpoint strategy**: `CheckpointTrigger` maps events to Light/Heavy tier.
//! - **Semantic idempotency**: Keys optionally include `workspace_version` + `memory_snapshot_id`.
//! - **IdempotencyCache trait**: pluggable backends (InMemory, MatrixOne).
//! - **Wait triggers**: `WaitTrigger` (User/Webhook/Timer) with `continuation_token`.
//! - **DB-first events**: `StepEventStore` trait (in-memory or MatrixOne).
//! - **Tool-level retry**: `ToolRetryPolicy` per tool classification.
//! - **Memory governance**: `MemoryGovernanceAction` for retrieval/promotion/purge tracking.
//! - **Migration**: `MigrationRegistry` for version upgrade hooks.
//!
//! # Hardening additions
//!
//! - **Memory governance**: `MemoryGovernanceAction` enum carried in `MemoryContext` for lifecycle tracking.
//! - **IdempotencyCache trait**: Abstraction over in-memory and MatrixOne-backed caches.
//! - **Checkpoint triggers**: `CheckpointTrigger` / `CheckpointTier` for strategy-driven checkpointing.
//! - **Canonical idempotency keys**: `compute_idempotency_key` uses `canonical_json` for determinism.
//! - **Migration registry**: `MigrationRegistry` for `VersionPolicy::Migrate` checkpoint upgrades.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use astra_text_utils::str_preview::prefix_chars;

// ─── Protocol Version ────────────────────────────────────────────────────────

/// Version encoding: major * 1000 + minor. E.g., 1000 = v1.0, 1001 = v1.1, 2000 = v2.0.
/// This makes Compatible (same major) and Migrate (major N-1) semantics meaningful.
pub const PROTOCOL_VERSION_MAJOR: u32 = 1;
pub const PROTOCOL_VERSION_MINOR: u32 = 0;
pub const PROTOCOL_VERSION: u32 = PROTOCOL_VERSION_MAJOR * 1000 + PROTOCOL_VERSION_MINOR;

/// How to handle version mismatches on checkpoint restore.
/// Negotiation chain: Strict → Compatible → Migrate → Discard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum VersionPolicy {
    /// Reject any mismatch (safe default for production)
    #[default]
    Strict,
    /// Accept if major version matches (same major = version / 1000).
    /// E.g., v1.0 (1000) and v1.1 (1001) are compatible.
    Compatible,
    /// Try compatible decode → try N-1 migration → discard.
    /// Recommended for long-lived deployments with registered MigrationFn.
    Migrate,
}

// ─── Migration Registry ──────────────────────────────────────────────────────

/// Type alias for migration functions.
/// Input: (source_version, checkpoint_json) → Result<migrated_json, error_message>
pub type MigrationFn = fn(u32, &serde_json::Value) -> Result<serde_json::Value, String>;

/// Registry of version migrations (for VersionPolicy::Migrate).
#[derive(Debug, Default)]
pub struct MigrationRegistry {
    /// Map from source_version → migration function
    migrations: HashMap<u32, MigrationFn>,
}

impl MigrationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, from_version: u32, f: MigrationFn) {
        self.migrations.insert(from_version, f);
    }

    pub fn migrate(
        &self,
        from_version: u32,
        data: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        if let Some(f) = self.migrations.get(&from_version) {
            f(from_version, data)
        } else {
            Err(format!(
                "No migration registered for version {}",
                from_version
            ))
        }
    }

    pub fn has_migration(&self, from_version: u32) -> bool {
        self.migrations.contains_key(&from_version)
    }

    /// Create a registry with built-in migrations.
    ///
    /// Currently registers:
    /// - v0 → v1000: legacy checkpoint upgrade (adds protocol_version field)
    pub fn with_defaults() -> Self {
        let mut reg = Self::new();
        reg.register(0, migrate_v0_to_v1000);
        reg
    }
}

/// Migration: v0 (pre-versioning) → v1000.
/// Adds `protocol_version` field if missing.
fn migrate_v0_to_v1000(_from: u32, data: &serde_json::Value) -> Result<serde_json::Value, String> {
    let mut migrated = data.clone();
    if let Some(obj) = migrated.as_object_mut() {
        // Add protocol_version to the light checkpoint (or top level)
        if !obj.contains_key("protocol_version") {
            obj.insert(
                "protocol_version".to_string(),
                serde_json::json!(PROTOCOL_VERSION),
            );
        }
        // If this is a Heavy checkpoint, ensure the inner light has it too
        if let Some(light) = obj.get_mut("light")
            && let Some(light_obj) = light.as_object_mut()
            && !light_obj.contains_key("protocol_version")
        {
            light_obj.insert(
                "protocol_version".to_string(),
                serde_json::json!(PROTOCOL_VERSION),
            );
        }
        Ok(migrated)
    } else {
        Err("checkpoint data is not a JSON object".to_string())
    }
}

/// Result of version negotiation (for Compatible/Migrate policies).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionVerdict {
    /// Exact match — proceed normally
    ExactMatch,
    /// Compatible (same major, different minor) — proceed with caution
    CompatibleDecode { found: u32 },
    /// Migrated from older version — proceed, data may be lossy
    Migrated { from: u32, to: u32 },
}

pub fn check_protocol_version_with_policy(
    version: u32,
    policy: VersionPolicy,
) -> Result<VersionVerdict, ProtocolError> {
    // Version 0 is always invalid regardless of policy
    if version == 0 {
        return Err(ProtocolError::VersionMismatch {
            expected: PROTOCOL_VERSION,
            found: 0,
            policy,
        });
    }

    // Exact match — always OK
    if version == PROTOCOL_VERSION {
        return Ok(VersionVerdict::ExactMatch);
    }

    match policy {
        VersionPolicy::Strict => Err(ProtocolError::VersionMismatch {
            expected: PROTOCOL_VERSION,
            found: version,
            policy,
        }),
        VersionPolicy::Compatible => {
            let expected_major = PROTOCOL_VERSION / 1000;
            let found_major = version / 1000;
            if expected_major == found_major {
                Ok(VersionVerdict::CompatibleDecode { found: version })
            } else {
                Err(ProtocolError::VersionMismatch {
                    expected: PROTOCOL_VERSION,
                    found: version,
                    policy,
                })
            }
        }
        VersionPolicy::Migrate => {
            // Step 1: try compatible decode (same major)
            let expected_major = PROTOCOL_VERSION / 1000;
            let found_major = version / 1000;
            if expected_major == found_major {
                return Ok(VersionVerdict::CompatibleDecode { found: version });
            }
            // Step 2: try migration (major N-1 → N)
            if found_major + 1 == expected_major {
                return Ok(VersionVerdict::Migrated {
                    from: version,
                    to: PROTOCOL_VERSION,
                });
            }
            // Step 3: too old, discard
            Err(ProtocolError::VersionMismatch {
                expected: PROTOCOL_VERSION,
                found: version,
                policy,
            })
        }
    }
}

/// Convenience: strict check (returns Ok(()) for backward compat)
pub fn check_protocol_version(version: u32) -> Result<(), ProtocolError> {
    check_protocol_version_with_policy(version, VersionPolicy::Strict).map(|_| ())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProtocolError {
    VersionMismatch {
        expected: u32,
        found: u32,
        policy: VersionPolicy,
    },
    InvalidCursor(String),
    CheckpointCorrupt(String),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VersionMismatch {
                expected,
                found,
                policy,
            } => {
                let action = match policy {
                    VersionPolicy::Strict => "Discard checkpoint and restart",
                    VersionPolicy::Compatible => "Incompatible major version, discarding",
                    VersionPolicy::Migrate => "No migration path, discarding",
                };
                write!(
                    f,
                    "Protocol version mismatch: expected v{expected}, found v{found} \
                     (policy: {policy:?}). {action}."
                )
            }
            Self::InvalidCursor(msg) => write!(f, "Invalid execution cursor: {msg}"),
            Self::CheckpointCorrupt(msg) => write!(f, "Corrupt checkpoint: {msg}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// Schema-level validation errors raised by `HeavyCheckpoint::validate_with`.
///
/// Distinct from `ProtocolError` because schema drift of embedded serialized
/// payloads (e.g. `continuity_state`) is an application-level concern the
/// runtime injects as a validator closure — pipeline does not own that schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// Base protocol validation failed (delegated from `validate()`).
    Protocol(String),
    /// Embedded `continuity_state` blob failed injected schema validator.
    ContinuityStateSchema(String),
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol(msg) => write!(f, "Protocol validation failed: {msg}"),
            Self::ContinuityStateSchema(msg) => {
                write!(f, "continuity_state schema validation failed: {msg}")
            }
        }
    }
}

impl std::error::Error for ValidationError {}

impl From<ProtocolError> for ValidationError {
    fn from(e: ProtocolError) -> Self {
        Self::Protocol(e.to_string())
    }
}

// ─── Step: Layered Structure ─────────────────────────────────────────────────

/// Scheduling contract — immutable policy governing a step's execution.
/// Attached to StepDescriptor at creation, enforced by the runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulingContract {
    /// Execution priority (0=background, 5=normal, 10=urgent).
    /// Higher priority steps execute first when multiple are queued.
    pub priority: u32,
    /// Maximum wall-clock time for the entire step (all tools combined).
    pub timeout_ms: u64,
    /// Per-tool timeout (0 = inherit from step timeout / tool_count).
    pub per_tool_timeout_ms: u64,
    /// Maximum retry attempts for transient failures.
    pub max_retries: u32,
    /// Initial backoff delay for retries (exponential: base * 2^attempt).
    pub backoff_base_ms: u64,
    /// Maximum backoff delay cap.
    pub backoff_max_ms: u64,
}

impl Default for SchedulingContract {
    fn default() -> Self {
        Self {
            priority: 5,
            timeout_ms: 300_000,
            per_tool_timeout_ms: 0,
            max_retries: 2,
            backoff_base_ms: 500,
            backoff_max_ms: 5_000,
        }
    }
}

impl SchedulingContract {
    /// Compute backoff delay for retry attempt N (exponential with cap).
    pub fn backoff_ms(&self, attempt: u32) -> u64 {
        let delay = self.backoff_base_ms.saturating_mul(1u64 << attempt.min(10));
        delay.min(self.backoff_max_ms)
    }

    /// Effective per-tool timeout: explicit value, or step timeout / tool_count.
    /// Floor: never less than 30s (30_000ms) to avoid starving individual tools
    /// when many tools share a step budget.
    pub fn effective_tool_timeout_ms(&self, tool_count: usize) -> u64 {
        const MIN_TOOL_TIMEOUT_MS: u64 = 30_000;
        if self.per_tool_timeout_ms > 0 {
            self.per_tool_timeout_ms
        } else if tool_count > 0 {
            (self.timeout_ms / tool_count as u64).max(MIN_TOOL_TIMEOUT_MS)
        } else {
            self.timeout_ms
        }
    }
}

/// Scheduling descriptor (immutable after creation, owned by Scheduler).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepDescriptor {
    pub step_id: String,
    pub task_id: String,
    pub dag_node_id: String,
    pub parent_step_id: Option<String>,
    pub action: StepAction,
    pub agent_id: Option<String>,
    /// Scheduling contract governing this step's execution policy.
    pub scheduling: SchedulingContract,
    pub protocol_version: u32,
    pub created_at: u64,
}

/// Runtime execution state (mutable during execution, owned by Agent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepExecution {
    pub cursor: ExecutionCursor,
    pub payload: StepPayload,
    pub result: Option<StepResult>,
    pub status: StepStatus,
    pub attempt: u32,
    pub max_attempts: u32,
    /// Memory context flowing through step lifecycle
    pub memory_context: Option<MemoryContext>,
    pub started_at: Option<u64>,
    pub completed_at: Option<u64>,
}

/// Memory context injected into steps (from PERCEIVE, used through ACT/EVALUATE).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryContext {
    /// IDs of memories retrieved in PERCEIVE
    pub retrieved_memory_ids: Vec<String>,
    /// Domain hints extracted from memory
    pub domain_hints: Vec<String>,
    /// Boost terms for tool selection
    pub boost_terms: Vec<String>,
    /// Provenance: which memories influenced this step
    pub provenance: Vec<String>,
    /// Memory governance actions triggered during this step
    #[serde(default)]
    pub governance_actions: Vec<MemoryGovernanceAction>,
    /// Cluster analysis results (from reflect/consolidate)
    #[serde(default)]
    pub cluster_insights: Vec<String>,
    /// Memory snapshot ID at step start (for diff detection)
    #[serde(default)]
    pub snapshot_id: Option<String>,
}

/// Memory governance actions carried through the step lifecycle.
/// Steps can trigger retrieval, promotion, purge, correction, or analysis.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MemoryGovernanceAction {
    /// Memory retrieved and used (provenance tracking)
    Retrieved { memory_id: String },
    /// Memory promoted from working to semantic
    Promoted { memory_id: String, reason: String },
    /// Memory purged (with reason)
    Purged { memory_id: String, reason: String },
    /// Memory corrected (with old/new summary)
    Corrected { memory_id: String, reason: String },
    /// Cluster analysis triggered
    ClusterAnalyzed { cluster_count: usize },
    /// Reflection triggered (episodic summary)
    Reflected { summary: String },
}

/// Composite Step = descriptor + execution + idempotency key.
/// This is the full Step passed between Scheduler and Agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub descriptor: StepDescriptor,
    pub execution: StepExecution,
    pub idempotency_key: String,
    pub checkpoint: Option<StepCheckpoint>,
}

impl Step {
    pub fn new(
        step_id: String,
        task_id: String,
        dag_node_id: String,
        action: StepAction,
        payload: StepPayload,
    ) -> Self {
        let idempotency_key = compute_idempotency_key(&task_id, &dag_node_id, &action, &payload);
        Self {
            descriptor: StepDescriptor {
                step_id,
                task_id,
                dag_node_id,
                parent_step_id: None,
                action,
                agent_id: None,
                scheduling: SchedulingContract::default(),
                protocol_version: PROTOCOL_VERSION,
                created_at: epoch_ms(),
            },
            execution: StepExecution {
                cursor: ExecutionCursor::default(),
                payload,
                result: None,
                status: StepStatus::Pending,
                attempt: 1,
                max_attempts: 3,
                memory_context: None,
                started_at: None,
                completed_at: None,
            },
            idempotency_key,
            checkpoint: None,
        }
    }

    // ── Convenience accessors ──

    pub fn step_id(&self) -> &str {
        &self.descriptor.step_id
    }

    pub fn task_id(&self) -> &str {
        &self.descriptor.task_id
    }

    pub fn action(&self) -> StepAction {
        self.descriptor.action
    }

    pub fn status(&self) -> StepStatus {
        self.execution.status
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self.execution.status,
            StepStatus::Completed | StepStatus::Failed | StepStatus::Cancelled
        )
    }

    pub fn is_retriable(&self) -> bool {
        self.execution.attempt < self.execution.max_attempts
            && !matches!(
                self.execution.status,
                StepStatus::Completed | StepStatus::Cancelled
            )
    }

    pub fn mark_started(&mut self, agent_id: &str) {
        self.descriptor.agent_id = Some(agent_id.to_string());
        self.execution.status = StepStatus::Running;
        self.execution.started_at = Some(epoch_ms());
    }

    pub fn mark_completed(&mut self, result: StepResult) {
        self.execution.result = Some(result);
        self.execution.status = StepStatus::Completed;
        self.execution.completed_at = Some(epoch_ms());
    }

    pub fn mark_failed(&mut self, error: &str) {
        self.execution.result = Some(StepResult::Error {
            message: error.to_string(),
        });
        self.execution.status = StepStatus::Failed;
        self.execution.completed_at = Some(epoch_ms());
    }

    pub fn with_memory_context(mut self, ctx: MemoryContext) -> Self {
        self.execution.memory_context = Some(ctx);
        self
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.descriptor.scheduling.timeout_ms = timeout_ms;
        self
    }

    pub fn with_scheduling(mut self, contract: SchedulingContract) -> Self {
        self.descriptor.scheduling = contract;
        self
    }
}

// ─── Step Action ─────────────────────────────────────────────────────────────

/// Execution actions. Decoupled from the UI "Turn" concept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StepAction {
    /// Intent recognition + memory retrieval
    Perceive,
    /// Tool selection + strategy
    Plan,
    /// LLM + tool execution
    Act,
    /// Progress evaluation + verdict
    Evaluate,
    /// Wait for external input (user, webhook, etc.)
    Wait,
    /// Terminal: success
    Done,
    /// Terminal: failure
    Fail,
}

impl StepAction {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Fail)
    }
}

impl std::fmt::Display for StepAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Perceive => write!(f, "PERCEIVE"),
            Self::Plan => write!(f, "PLAN"),
            Self::Act => write!(f, "ACT"),
            Self::Evaluate => write!(f, "EVALUATE"),
            Self::Wait => write!(f, "WAIT"),
            Self::Done => write!(f, "DONE"),
            Self::Fail => write!(f, "FAIL"),
        }
    }
}

// ─── Step Status ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StepStatus {
    Pending,
    Assigned,
    Running,
    Completed,
    Failed,
    TimedOut,
    Cancelled,
}

impl StepStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Assigned => "ASSIGNED",
            Self::Running => "RUNNING",
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
            Self::TimedOut => "TIMED_OUT",
            Self::Cancelled => "CANCELLED",
        }
    }
}

// ─── Execution Cursor (Slot-based) ───────────────────────────────────────────

/// Precise execution position within a Step.
/// Uses `ExecutionSlot` per tool — each is an independent state machine.
/// Supports both sequential and parallel dispatch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionCursor {
    /// Current phase being executed
    pub phase: StepAction,
    /// Per-tool execution slots (ACT phase).
    /// Each slot is an independent state machine — no shared index.
    pub slots: Vec<ExecutionSlot>,
    /// Execution mode: sequential (dispatch one at a time) or parallel (all at once)
    pub parallel: bool,
    /// For Wait steps: how to resume
    pub wait_trigger: Option<WaitTrigger>,
    /// Sub-step identifier (future: nested/composite steps)
    pub sub_step: Option<String>,
}

/// Per-tool execution slot — independent state machine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionSlot {
    /// Slot index (0-based, stable after creation)
    pub index: u32,
    /// Tool name (set when dispatched)
    pub tool_name: String,
    /// Unique call ID (correlates with LLM tool_call id)
    pub call_id: String,
    /// Current state of this slot
    pub state: SlotState,
    /// Points to idempotency cache entry
    pub idempotency_key: Option<String>,
    /// Stable, short preview of the tool arguments for trace/debugging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_preview: Option<String>,
    /// Inline cached result (for checkpoint completeness)
    pub cached_result: Option<CachedToolResult>,
    /// Tool-level retry count (separate from step retry)
    pub retry_count: u32,
}

/// Slot state machine: Pending → Running → Completed|Failed|Skipped
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlotState {
    /// Not yet dispatched
    Pending,
    /// Currently executing (crash recovery point)
    Running,
    /// Completed successfully, result cached
    Completed,
    /// Execution failed
    Failed,
    /// Skipped (dedup hit or conditional)
    Skipped,
}

/// How a Wait step should be resumed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WaitTrigger {
    /// What kind of event resumes execution
    pub trigger_type: WaitTriggerType,
    /// Opaque token for async resume (e.g., webhook callback URL, timer ID)
    pub continuation_token: String,
    /// Maximum wait before auto-timeout (None = wait indefinitely)
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WaitTriggerType {
    /// Waiting for user input (interactive prompt)
    User,
    /// Waiting for external webhook callback
    Webhook,
    /// Waiting for a timer (scheduled delay)
    Timer,
    /// Waiting for another step/task to complete
    Dependency,
}

impl Default for ExecutionCursor {
    fn default() -> Self {
        Self {
            phase: StepAction::Perceive,
            slots: Vec::new(),
            parallel: false,
            wait_trigger: None,
            sub_step: None,
        }
    }
}

impl ExecutionCursor {
    /// Create cursor for an ACT step with N tool calls (sequential dispatch)
    pub fn for_act(num_tools: usize) -> Self {
        Self {
            phase: StepAction::Act,
            slots: (0..num_tools)
                .map(|i| ExecutionSlot {
                    index: i as u32,
                    tool_name: String::new(),
                    call_id: String::new(),
                    state: SlotState::Pending,
                    idempotency_key: None,
                    args_preview: None,
                    cached_result: None,
                    retry_count: 0,
                })
                .collect(),
            parallel: false,
            wait_trigger: None,
            sub_step: None,
        }
    }

    /// Create cursor for parallel tool execution (all dispatched simultaneously)
    pub fn for_parallel_act(num_tools: usize) -> Self {
        let mut cursor = Self::for_act(num_tools);
        cursor.parallel = true;
        cursor
    }

    /// Create cursor for Wait step with typed trigger
    pub fn for_wait(trigger: WaitTrigger) -> Self {
        Self {
            phase: StepAction::Wait,
            wait_trigger: Some(trigger),
            ..Self::default()
        }
    }

    /// Advance a specific slot's state (by index)
    pub fn advance_slot(&mut self, index: usize, state: SlotState) {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.state = state;
        }
    }

    /// Get next pending slot index (for sequential dispatch)
    pub fn next_pending_slot(&self) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| s.state == SlotState::Pending)
    }

    /// Are all execution slots resolved (not Pending or Running)?
    pub fn all_slots_done(&self) -> bool {
        self.slots.iter().all(|s| {
            matches!(
                s.state,
                SlotState::Completed | SlotState::Failed | SlotState::Skipped
            )
        })
    }

    /// Count of completed slots
    pub fn completed_slot_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.state == SlotState::Completed)
            .count()
    }

    /// Count of pending slots
    pub fn pending_slot_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.state == SlotState::Pending)
            .count()
    }

    /// Count of failed slots
    pub fn failed_slot_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.state == SlotState::Failed)
            .count()
    }
}

// ─── Checkpoint (Tiered: Light / Heavy) ──────────────────────────────────────

/// Light checkpoint: cursor + metadata only.
/// Written frequently (every tool completion), cheap to serialize.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LightCheckpoint {
    pub protocol_version: u32,
    pub cursor: ExecutionCursor,
    pub step_id: String,
    pub task_id: String,
    pub agent_id: String,
    pub progress: f64,
    pub total_tokens: u64,
    pub created_at: u64,
}

/// Heavy checkpoint: light + full conversation state + tool results.
/// Written infrequently (phase transitions, before expensive operations).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeavyCheckpoint {
    /// All fields from light checkpoint
    pub light: LightCheckpoint,
    /// Full conversation messages (for LLM resume)
    pub messages: Vec<serde_json::Value>,
    /// Token budget state
    pub budget_remaining_tokens: u64,
    pub budget_remaining_rounds: u32,
    /// Session state
    pub blocked_tools: Vec<String>,
    pub recent_tools: Vec<String>,
    /// Learning state reference
    pub learning_snapshot_id: Option<String>,
    /// Memory context snapshot (for auditing)
    pub memory_context: Option<MemoryContext>,
    /// Active delegation ID (if running inside a delegation)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation_id: Option<String>,
    /// Delegation pattern (fan_out, sequential, adversarial)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation_pattern: Option<String>,
    /// Completed sub-run summaries for delegation recovery
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delegation_sub_run_summaries: Vec<DelegationSubRunSummary>,
    /// Structured interruption record — captures why the session was interrupted
    /// and what the caller should do to resume. Present only when the checkpoint
    /// was written in response to an interruption (budget exhaustion, rate limit,
    /// context overflow, cancellation, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interruption: Option<serde_json::Value>,
    /// Serialized approval overrides (FingerprintedOverrides) for session continuity.
    /// When restored, merged into the live PermissionManager so approval decisions
    /// survive session restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_overrides: Option<serde_json::Value>,
    /// Consecutive context-window errors counter for compaction tier escalation.
    /// Persisted so aggressive-tier compaction survives session resume.
    #[serde(default)]
    pub consecutive_context_window_errors: u32,
    /// Serialized context pipeline state (PipelineStats + SessionLatches + RecoveryState).
    /// Enables warm-start on session resume: EMA cache ratios, percentile reserves,
    /// latched headers/scope, and output escalation history survive across sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline_state: Option<serde_json::Value>,
    /// Serialized CompactionEffectivenessTracker state for cross-turn persistence.
    /// Contains cumulative_tokens_freed, attempt_count, last_tokens_freed,
    /// last_was_insufficient — enabling enriched resume guidance and tier selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_state: Option<serde_json::Value>,
    /// Serialized runtime-owned continuity state (goal/todo/facts/attention).
    /// Restored before the next model call so "continue" does not depend on
    /// LLM narrative memory or explicit task-tool usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuity_state: Option<serde_json::Value>,
}
/// Summary of a completed delegation sub-run, stored in HeavyCheckpoint for recovery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelegationSubRunSummary {
    pub run_id: String,
    pub agent_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub tool_calls: u32,
}

/// A named breakpoint — an addressable point in the execution timeline.
/// Wraps a `HeavyCheckpoint` with additional metadata for resume/fork.
///
/// The optional `composite_snapshot` replaces the old flat fields with a
/// unified bag-of-references model. When present, `tool_health_entries`,
/// `learning_snapshot_epoch` etc. are still populated for backward compat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BreakpointInfo {
    /// Unique identifier for this breakpoint.
    pub breakpoint_id: String,
    /// Session ID this breakpoint belongs to.
    pub session_id: String,
    /// Turn number at the time of breakpoint.
    pub turn_number: u32,
    /// Checkpoint number (maps to step_checkpoints/<NNN>-heavy.json).
    pub checkpoint_number: u32,
    /// Human-readable label (auto-generated or user-provided).
    pub label: String,
    /// ISO 8601 timestamp.
    pub created_at: String,
    /// Tool health state at this point (for cross-session persistence).
    pub tool_health_entries: Vec<crate::ToolHealthEntry>,
    /// Correction history from TurnGuard at this point.
    pub correction_history_json: Option<String>,
    /// Learning snapshot identifier (profile name + epoch).
    pub learning_snapshot_epoch: Option<u64>,
    /// Composite snapshot — unified bag-of-references across state dimensions.
    /// When present, this is the canonical source; the flat fields above are kept
    /// for backward compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composite_snapshot: Option<astra_core::composite_snapshot::CompositeSnapshot>,
}

/// Index of all breakpoints in a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BreakpointIndex {
    pub breakpoints: Vec<BreakpointInfo>,
}

// ─── Composite Snapshot (re-exported from astra-core) ────────────────────────

pub use astra_core::composite_snapshot::{
    CompositeSnapshot, CompositeSnapshotIndex, DataSnapshotRef, MemorySnapshotRef, SnapshotRef,
    SnapshotSpec,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StepCheckpoint {
    Light(LightCheckpoint),
    Heavy(Box<HeavyCheckpoint>),
}

impl StepCheckpoint {
    /// Create a light checkpoint
    pub fn light(
        step_id: String,
        task_id: String,
        agent_id: String,
        cursor: ExecutionCursor,
    ) -> Self {
        Self::Light(LightCheckpoint {
            protocol_version: PROTOCOL_VERSION,
            cursor,
            step_id,
            task_id,
            agent_id,
            progress: 0.0,
            total_tokens: 0,
            created_at: epoch_ms(),
        })
    }

    /// Create a heavy checkpoint (full recovery point)
    pub fn heavy(
        step_id: String,
        task_id: String,
        agent_id: String,
        cursor: ExecutionCursor,
    ) -> Self {
        Self::Heavy(Box::new(HeavyCheckpoint {
            light: LightCheckpoint {
                protocol_version: PROTOCOL_VERSION,
                cursor,
                step_id,
                task_id,
                agent_id,
                progress: 0.0,
                total_tokens: 0,
                created_at: epoch_ms(),
            },
            messages: Vec::new(),
            budget_remaining_tokens: 0,
            budget_remaining_rounds: 0,
            blocked_tools: Vec::new(),
            recent_tools: Vec::new(),
            learning_snapshot_id: None,
            memory_context: None,
            delegation_id: None,
            delegation_pattern: None,
            delegation_sub_run_summaries: Vec::new(),
            interruption: None,
            approval_overrides: None,
            consecutive_context_window_errors: 0,
            pipeline_state: None,
            compaction_state: None,
            continuity_state: None,
        }))
    }

    /// Convenience constructor (creates Heavy by default).
    pub fn new(
        step_id: String,
        task_id: String,
        agent_id: String,
        cursor: ExecutionCursor,
    ) -> Self {
        Self::heavy(step_id, task_id, agent_id, cursor)
    }

    /// Get protocol version from either tier
    pub fn protocol_version(&self) -> u32 {
        match self {
            Self::Light(l) => l.protocol_version,
            Self::Heavy(h) => h.light.protocol_version,
        }
    }

    /// Get cursor reference from either tier
    pub fn cursor(&self) -> &ExecutionCursor {
        match self {
            Self::Light(l) => &l.cursor,
            Self::Heavy(h) => &h.light.cursor,
        }
    }

    /// Validate checkpoint before restoring. Checks:
    /// 1. Protocol version compatibility
    /// 2. ACT phase must have execution slots
    /// 3. Wait phase must have a wait_trigger
    /// 4. Running slots are invalid in checkpoint (crash = reset to Pending)
    /// 5. If Heavy, messages must not be empty for non-Perceive phases
    pub fn validate(&self) -> Result<(), ProtocolError> {
        check_protocol_version(self.protocol_version())?;
        let cursor = self.cursor();

        // ACT phase requires slots
        if cursor.phase == StepAction::Act && cursor.slots.is_empty() {
            return Err(ProtocolError::InvalidCursor(
                "ACT phase cursor has no execution slots".into(),
            ));
        }

        // Wait phase requires a trigger
        if cursor.phase == StepAction::Wait && cursor.wait_trigger.is_none() {
            return Err(ProtocolError::InvalidCursor(
                "WAIT phase cursor has no wait_trigger".into(),
            ));
        }

        // Running slots in checkpoint = crash artifact; must be reset before restore
        let running_count = cursor
            .slots
            .iter()
            .filter(|s| s.state == SlotState::Running)
            .count();
        if running_count > 0 {
            return Err(ProtocolError::CheckpointCorrupt(format!(
                "{running_count} slot(s) still in Running state (crash before completion)"
            )));
        }

        // Heavy checkpoint: non-Perceive phases should have messages for LLM resume
        if let Self::Heavy(h) = self
            && h.light.cursor.phase != StepAction::Perceive
            && h.messages.is_empty()
        {
            return Err(ProtocolError::CheckpointCorrupt(
                "Heavy checkpoint has no messages for non-Perceive phase".into(),
            ));
        }

        Ok(())
    }

    /// Is this a heavy (full recovery) checkpoint?
    pub fn is_heavy(&self) -> bool {
        matches!(self, Self::Heavy(_))
    }

    /// Extended validation with an injected schema validator for the
    /// embedded `continuity_state` blob.
    ///
    /// The validator closure is **only** invoked when:
    ///   - `self` is a `Heavy` checkpoint, AND
    ///   - `continuity_state` is `Some(_)`.
    ///
    /// Light checkpoints do not carry embedded continuity state; for `Light`
    /// variants this method still runs base protocol validation, then treats the
    /// injected schema validator as a no-op.
    ///
    /// This keeps `astra-pipeline` free of any knowledge about the continuity
    /// schema (which lives in `astra-turn-types`), while still giving restore
    /// paths a single choke-point to reject malformed blobs at checkpoint
    /// validation time rather than discovering drift later during use.
    pub fn validate_with<F>(&self, validator: F) -> Result<(), ValidationError>
    where
        F: FnOnce(&serde_json::Value) -> Result<(), String>,
    {
        self.validate()?;
        if let Self::Heavy(h) = self {
            h.validate_continuity_with(validator)?;
        }
        Ok(())
    }
}

impl HeavyCheckpoint {
    /// Schema-level validator for `HeavyCheckpoint` that invokes the provided
    /// closure on `continuity_state` when present.
    ///
    /// The validator closure is **only** invoked when `continuity_state` is
    /// `Some(_)`. This keeps `astra-pipeline` free of any knowledge about the
    /// continuity schema (which lives in `astra-turn-types`), while still giving
    /// restore paths a single choke-point to reject malformed blobs at
    /// checkpoint validation time rather than discovering drift later during
    /// use.
    pub fn validate_with<F>(&self, validator: F) -> Result<(), ValidationError>
    where
        F: FnOnce(&serde_json::Value) -> Result<(), String>,
    {
        self.validate_continuity_with(validator)
    }

    fn validate_continuity_with<F>(&self, validator: F) -> Result<(), ValidationError>
    where
        F: FnOnce(&serde_json::Value) -> Result<(), String>,
    {
        if let Some(cs) = &self.continuity_state {
            validator(cs).map_err(ValidationError::ContinuityStateSchema)?;
        }
        Ok(())
    }
}

// ─── Checkpoint Trigger Strategy ─────────────────────────────────────────────

/// When to write checkpoints. Enforced by the execution engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckpointTrigger {
    /// After every slot completion → LightCheckpoint
    SlotCompleted,
    /// On phase transition (Perceive→Plan→Act→Evaluate) → HeavyCheckpoint
    PhaseTransition,
    /// Before expensive operations (LLM call, bash) → LightCheckpoint
    BeforeExpensiveOp,
    /// Explicit user/system request → HeavyCheckpoint
    Explicit,
}

impl CheckpointTrigger {
    /// What tier of checkpoint should this trigger produce?
    pub fn checkpoint_tier(&self) -> CheckpointTier {
        match self {
            Self::SlotCompleted | Self::BeforeExpensiveOp => CheckpointTier::Light,
            Self::PhaseTransition | Self::Explicit => CheckpointTier::Heavy,
        }
    }
}

/// Tier of checkpoint produced by a trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckpointTier {
    Light,
    Heavy,
}

// ─── Payload & Result ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StepPayload {
    Perceive {
        user_query: String,
        memory_context: Vec<String>,
    },
    Plan {
        intent_signals: Vec<String>,
        intent_confidence: f64,
        available_tool_count: usize,
        budget_tokens: u64,
        restricted_tools: Vec<String>,
    },
    Act {
        selected_tools: Vec<String>,
        tool_calls: Vec<serde_json::Value>,
    },
    Evaluate {
        tool_results_count: usize,
        progress_history: Vec<f64>,
        budget_remaining_tokens: u64,
    },
    Wait {
        prompt: String,
        choices: Option<Vec<String>>,
        timeout_ms: Option<u64>,
    },
    Terminal {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StepResult {
    Perceive {
        intent_signals: Vec<String>,
        intent_confidence: f64,
        entities: Vec<String>,
        memory_matches: usize,
        boost_terms: Vec<String>,
    },
    Plan {
        selected_tools: Vec<String>,
        confidence: f64,
    },
    Act {
        tool_results_count: usize,
        assistant_text: Option<String>,
        tokens_in: u64,
        tokens_out: u64,
    },
    Evaluate {
        verdict: StepVerdict,
        progress: f64,
        should_continue: bool,
        next_action: StepAction,
    },
    Wait {
        response: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepVerdict {
    Continue,
    Stalled,
    Diverging,
    Complete,
    Failed,
    BudgetExhausted,
}

// ─── Retry Policy ────────────────────────────────────────────────────────────

/// Default absolute ceiling for automatic retries (step + tool policies, serde default).
pub const DEFAULT_RETRY_MAX_ATTEMPTS_CEILING: u32 = 5;

/// Step-level retry policy (fallback when tool-level not specified).
fn default_retry_max_retries() -> u32 {
    DEFAULT_RETRY_MAX_ATTEMPTS_CEILING
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    /// Absolute ceiling on step-level retry attempts (defense in depth vs misconfigured `max_attempts`).
    #[serde(default = "default_retry_max_retries")]
    pub max_retries: u32,
    pub backoff_base_ms: u64,
    pub backoff_max_ms: u64,
    pub retry_on: Vec<ErrorCategory>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            max_retries: default_retry_max_retries(),
            backoff_base_ms: 500,
            backoff_max_ms: 30_000,
            retry_on: vec![ErrorCategory::Transient, ErrorCategory::Timeout],
        }
    }
}

impl RetryPolicy {
    /// Compute backoff delay for attempt N (exponential with jitter cap)
    pub fn backoff_ms(&self, attempt: u32) -> u64 {
        let delay = self.backoff_base_ms.saturating_mul(1u64 << attempt.min(10));
        delay.min(self.backoff_max_ms)
    }

    pub fn should_retry(&self, attempt: u32, category: &ErrorCategory) -> bool {
        let limit = self.max_attempts.min(self.max_retries.max(1));
        attempt < limit && self.retry_on.contains(category)
    }
}

/// Tool-level retry policy (more granular than step-level).
/// A single tool failure doesn't force whole-step retry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRetryPolicy {
    pub max_attempts: u32,
    #[serde(default = "default_retry_max_retries")]
    pub max_retries: u32,
    pub backoff_base_ms: u64,
    pub backoff_max_ms: u64,
}

impl Default for ToolRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 2,
            max_retries: default_retry_max_retries(),
            backoff_base_ms: 300,
            backoff_max_ms: 5_000,
        }
    }
}

impl ToolRetryPolicy {
    pub fn backoff_ms(&self, attempt: u32) -> u64 {
        self.backoff_base_ms
            .saturating_mul(1u64 << attempt.min(10))
            .min(self.backoff_max_ms)
    }

    pub fn should_retry(&self, attempt: u32) -> bool {
        let limit = self.max_attempts.min(self.max_retries.max(1));
        attempt < limit
    }
}

/// Get tool-level retry policy based on idempotency classification.
pub fn tool_retry_policy(tool_name: &str) -> ToolRetryPolicy {
    match classify_tool_idempotency(tool_name) {
        // Pure reads: retry aggressively (no side effects)
        ToolIdempotency::PureRead => ToolRetryPolicy {
            max_attempts: 3,
            max_retries: default_retry_max_retries(),
            backoff_base_ms: 200,
            backoff_max_ms: 2_000,
        },
        // Idempotent writes: retry cautiously
        ToolIdempotency::IdempotentWrite => ToolRetryPolicy {
            max_attempts: 2,
            max_retries: default_retry_max_retries(),
            backoff_base_ms: 500,
            backoff_max_ms: 5_000,
        },
        // Non-idempotent: do NOT auto-retry (let LLM decide)
        ToolIdempotency::NonIdempotent => ToolRetryPolicy {
            max_attempts: 1, // no retry
            max_retries: 1,
            backoff_base_ms: 0,
            backoff_max_ms: 0,
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCategory {
    Transient,
    Timeout,
    RateLimit,
    AuthFailure,
    InvalidInput,
    ToolNotFound,
    InternalError,
}

// ─── Idempotency ─────────────────────────────────────────────────────────────

/// Key for idempotency cache lookup.
/// v3: optionally includes workspace context for precise dedup.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IdempotencyKey {
    /// Step identifier (empty for semantic keys)
    pub step_id: String,
    pub tool_index: u32,
    /// Content hash of tool_name + canonical_args
    pub content_hash: String,
    /// Optional context signature for more precise dedup
    pub context_signature: Option<ContextSignature>,
}

/// Optional context that affects idempotency (same tool+args may produce
/// different results if workspace or memory state changed).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContextSignature {
    /// Git HEAD or workspace version (e.g., commit SHA)
    pub workspace_version: Option<String>,
    /// Memory snapshot ID (if relevant memories changed, invalidate cache)
    pub memory_snapshot_id: Option<String>,
}

impl IdempotencyKey {
    /// Execution-level key: tied to specific step + tool index
    pub fn new(step_id: &str, tool_index: u32, tool_name: &str, args: &serde_json::Value) -> Self {
        let content_hash = compute_content_hash(tool_name, args);
        Self {
            step_id: step_id.to_string(),
            tool_index,
            content_hash,
            context_signature: None,
        }
    }

    /// Semantic-level key: content-only (for DAG reuse / step replay)
    pub fn semantic(tool_name: &str, args: &serde_json::Value) -> Self {
        let content_hash = compute_content_hash(tool_name, args);
        Self {
            step_id: String::new(),
            tool_index: 0,
            content_hash,
            context_signature: None,
        }
    }

    /// Attach workspace/memory context for more precise dedup
    pub fn with_context(mut self, ctx: ContextSignature) -> Self {
        self.context_signature = Some(ctx);
        self
    }

    /// Cache key used for HashMap lookup.
    /// Includes context_signature if present (to invalidate on workspace change).
    pub fn cache_key(&self) -> String {
        let base = if self.step_id.is_empty() {
            format!("sem:{}", self.content_hash)
        } else {
            format!("{}:{}:{}", self.step_id, self.tool_index, self.content_hash)
        };
        if let Some(ctx) = &self.context_signature {
            let mut parts = base;
            if let Some(ws) = &ctx.workspace_version {
                parts.push_str(&format!(":ws={}", prefix_chars(ws, 8)));
            }
            if let Some(ms) = &ctx.memory_snapshot_id {
                parts.push_str(&format!(":ms={}", prefix_chars(ms, 8)));
            }
            parts
        } else {
            base
        }
    }

    pub fn is_semantic(&self) -> bool {
        self.step_id.is_empty()
    }
}

/// Cached tool result (for crash recovery).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CachedToolResult {
    pub tool_name: String,
    pub output: String,
    pub is_error: bool,
    pub cached_at: u64,
}

pub use astra_turn_types::{ToolIdempotency, classify_tool_idempotency};

/// Trait for idempotency caches. InMemory for local, MatrixOne for cloud.
pub trait IdempotencyCache {
    /// Check if result is cached (returns owned value for trait-object safety)
    fn check(&self, key: &IdempotencyKey) -> Option<CachedToolResult>;
    /// Record a tool result
    fn record(&mut self, key: &IdempotencyKey, result: CachedToolResult);
    /// Remove all entries for a step (cleanup after step completes)
    fn evict_step(&mut self, step_id: &str);
    /// Number of cached entries
    fn len(&self) -> usize;
    /// Whether the cache is empty
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// In-memory idempotency cache (v2-v3; v4 uses MatrixOne).
#[derive(Debug, Default)]
pub struct InMemoryIdempotencyCache {
    cache: HashMap<String, CachedToolResult>,
}

impl InMemoryIdempotencyCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if result is cached
    pub fn check(&self, key: &IdempotencyKey) -> Option<&CachedToolResult> {
        self.cache.get(&key.cache_key())
    }

    /// Record a tool result
    pub fn record(&mut self, key: &IdempotencyKey, result: CachedToolResult) {
        self.cache.insert(key.cache_key(), result);
    }

    /// Remove cached results for a tool. Used after workspace mutations to
    /// prevent stale read-only results from bypassing tool-level freshness checks.
    pub fn evict_tool(&mut self, tool_name: &str) {
        self.cache.retain(|_, result| result.tool_name != tool_name);
    }

    /// Remove cached results for any of the provided tools.
    pub fn evict_tools(&mut self, tool_names: &[&str]) {
        self.cache
            .retain(|_, result| !tool_names.contains(&result.tool_name.as_str()));
    }

    /// Remove all entries for a step (cleanup after step completes).
    /// Uses delimiter ":" to avoid prefix collisions (e.g., "s1" vs "s10").
    pub fn evict_step(&mut self, step_id: &str) {
        let prefix = format!("{}:", step_id);
        self.cache
            .retain(|k, _| !k.starts_with(&prefix) && k != step_id);
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}

impl IdempotencyCache for InMemoryIdempotencyCache {
    fn check(&self, key: &IdempotencyKey) -> Option<CachedToolResult> {
        self.cache.get(&key.cache_key()).cloned()
    }

    fn record(&mut self, key: &IdempotencyKey, result: CachedToolResult) {
        self.cache.insert(key.cache_key(), result);
    }

    fn evict_step(&mut self, step_id: &str) {
        let prefix = format!("{}:", step_id);
        self.cache
            .retain(|k, _| !k.starts_with(&prefix) && k != step_id);
    }

    fn len(&self) -> usize {
        self.cache.len()
    }
}

// ─── Step Event (DAG) — DB-first ─────────────────────────────────────────────

/// Event in the step execution DAG. Multi-parent support for
/// representing parallel tool execution converging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepEvent {
    pub event_id: String,
    pub step_id: String,
    pub event_type: StepEventType,
    pub agent_id: Option<String>,
    /// Multi-parent: enables DAG (not just chain)
    pub caused_by: Vec<String>,
    pub payload: Option<serde_json::Value>,
    pub created_at: u64,
}

/// DB-first event store trait. Primary storage = database,
/// in-memory is just a view/cache.
pub trait StepEventStore {
    /// Append event to the store
    fn append(&mut self, event: StepEvent);
    /// Query events for a step (ordered by created_at)
    fn events_for_step(&self, step_id: &str) -> Vec<&StepEvent>;
    /// Find all ancestors (BFS up the caused_by DAG)
    fn ancestors(&self, event_id: &str) -> Vec<&StepEvent>;
    /// Find all descendants (BFS down from event_id)
    fn descendants(&self, event_id: &str) -> Vec<&StepEvent>;
    /// Find leaf events (no children)
    fn leaves(&self) -> Vec<&StepEvent>;
    /// Total event count
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepEventType {
    StepCreated,
    StepAssigned,
    StepStarted,
    StepCompleted,
    /// Step ended without terminal success/failure, e.g. a visible turn paused,
    /// hit a round budget, or yielded to the next iteration.
    StepIncomplete,
    /// Step evaluation completed and the runtime decided what to do next.
    /// This is not a terminal event; terminal status is recorded separately
    /// via `StepCompleted` or `StepIncomplete` (from `end_turn()`).
    /// Early-exit paths that skip evaluation (retry, stop_hook, continue)
    /// intentionally omit `StepEvaluated` and emit only `StepIncomplete`.
    StepEvaluated,
    StepFailed,
    StepRetried,

    ToolCallStarted,
    ToolCallCompleted,
    ToolCallFailed,
    ToolCallSkipped,

    /// Multiple parallel tool calls converged into evaluation
    ToolsConverged,

    CheckpointSaved,
    CheckpointRestored,

    MemoryRetrieved,
    MemoryRecorded,
    MemoryGovernanceApplied,

    StallDetected,
    DivergenceDetected,
    RetryScheduled,
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

pub fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn compute_content_hash(tool_name: &str, args: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(tool_name.as_bytes());
    hasher.update(b":");
    // Canonical JSON: serde_json sorts map keys deterministically
    // (BTreeMap internally), so identical args → identical hash.
    let canonical = canonical_json(args);
    hasher.update(canonical.as_bytes());
    let hash = hasher.finalize();
    let hex = format!("{:x}", hash);
    prefix_chars(&hex, 16)
}

/// Produce canonical JSON with sorted keys (recursively).
/// Uses serde_json for key escaping to handle special characters correctly.
fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            sorted.sort_by_key(|(k, _)| *k);
            let entries: Vec<String> = sorted
                .iter()
                .map(|(k, v)| {
                    // Use serde_json for proper key escaping (handles ", \, etc.)
                    let escaped_key = serde_json::to_string(k.as_str()).unwrap_or_default();
                    format!("{}:{}", escaped_key, canonical_json(v))
                })
                .collect();
            format!("{{{}}}", entries.join(","))
        }
        serde_json::Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(canonical_json).collect();
            format!("[{}]", items.join(","))
        }
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn compute_idempotency_key(
    task_id: &str,
    dag_node_id: &str,
    action: &StepAction,
    payload: &StepPayload,
) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(task_id.as_bytes());
    hasher.update(b":");
    hasher.update(dag_node_id.as_bytes());
    hasher.update(b":");
    hasher.update(action.to_string().as_bytes());
    hasher.update(b":");
    // Convert to Value then canonical_json for deterministic output
    let payload_json = serde_json::to_string(payload).unwrap_or_default();
    let payload_value: serde_json::Value = serde_json::from_str(&payload_json).unwrap_or_default();
    hasher.update(canonical_json(&payload_value).as_bytes());
    let hash = hasher.finalize();
    let hex = format!("{:x}", hash);
    prefix_chars(&hex, 32) // 32-char hex prefix
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Protocol Version ──

    #[test]
    fn protocol_version_match_ok() {
        assert!(check_protocol_version(PROTOCOL_VERSION).is_ok());
    }

    #[test]
    fn protocol_version_mismatch_err() {
        let result = check_protocol_version(PROTOCOL_VERSION + 1);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ProtocolError::VersionMismatch { .. }));
        // Strict policy → "Discard checkpoint and restart"
        assert!(err.to_string().contains("Discard"));
    }

    #[test]
    fn protocol_version_zero_rejected() {
        assert!(check_protocol_version(0).is_err());
    }

    // ── Version Policy Negotiation Chain ──

    #[test]
    fn version_strict_rejects_mismatch() {
        let result = check_protocol_version_with_policy(999, VersionPolicy::Strict);
        assert!(result.is_err());
    }

    #[test]
    fn version_strict_accepts_exact() {
        let result = check_protocol_version_with_policy(PROTOCOL_VERSION, VersionPolicy::Strict);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), VersionVerdict::ExactMatch);
    }

    #[test]
    fn version_compatible_same_major() {
        // PROTOCOL_VERSION = 1000 (v1.0), major = 1. version 1050 → major = 1 (same)
        let result = check_protocol_version_with_policy(1050, VersionPolicy::Compatible);
        assert!(result.is_ok());
        assert!(matches!(
            result.unwrap(),
            VersionVerdict::CompatibleDecode { found: 1050 }
        ));
    }

    #[test]
    fn version_compatible_diff_major_rejects() {
        // version 2000 → major = 2 (different from PROTOCOL_VERSION major = 1)
        let result = check_protocol_version_with_policy(2000, VersionPolicy::Compatible);
        assert!(result.is_err());
        if let Err(ProtocolError::VersionMismatch { policy, .. }) = result {
            assert_eq!(policy, VersionPolicy::Compatible);
        } else {
            panic!("expected VersionMismatch");
        }
    }

    #[test]
    fn version_compatible_zero_rejected() {
        let result = check_protocol_version_with_policy(0, VersionPolicy::Compatible);
        assert!(result.is_err());
    }

    #[test]
    fn version_migrate_same_major_compat() {
        // version 1050 → same major (1) → CompatibleDecode
        let result = check_protocol_version_with_policy(1050, VersionPolicy::Migrate);
        assert!(result.is_ok());
        assert!(matches!(
            result.unwrap(),
            VersionVerdict::CompatibleDecode { .. }
        ));
    }

    #[test]
    fn version_migrate_prev_major() {
        // version 50 → major 0, expected major 1 → Migrated
        let result = check_protocol_version_with_policy(50, VersionPolicy::Migrate);
        assert!(result.is_ok());
        assert!(matches!(
            result.unwrap(),
            VersionVerdict::Migrated { from: 50, to: 1000 }
        ));
    }

    #[test]
    fn version_migrate_zero_rejected() {
        let result = check_protocol_version_with_policy(0, VersionPolicy::Migrate);
        assert!(result.is_err());
    }

    #[test]
    fn version_exact_match_all_policies() {
        for policy in [
            VersionPolicy::Strict,
            VersionPolicy::Compatible,
            VersionPolicy::Migrate,
        ] {
            let result = check_protocol_version_with_policy(PROTOCOL_VERSION, policy);
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), VersionVerdict::ExactMatch);
        }
    }

    // ── Step Lifecycle ──

    #[test]
    fn step_creation_defaults() {
        let step = Step::new(
            "s1".into(),
            "t1".into(),
            "n1".into(),
            StepAction::Perceive,
            StepPayload::Perceive {
                user_query: "hello".into(),
                memory_context: vec![],
            },
        );
        assert_eq!(step.status(), StepStatus::Pending);
        assert_eq!(step.descriptor.protocol_version, PROTOCOL_VERSION);
        assert_eq!(step.execution.attempt, 1);
        assert_eq!(step.execution.max_attempts, 3);
        assert!(!step.idempotency_key.is_empty());
        assert!(!step.is_terminal());
        assert!(step.is_retriable());
        assert!(step.descriptor.agent_id.is_none());
        assert!(step.execution.result.is_none());
        assert!(step.checkpoint.is_none());
        assert!(step.execution.memory_context.is_none());
    }

    #[test]
    fn step_creation_with_memory_context() {
        let step = Step::new(
            "s1".into(),
            "t1".into(),
            "n1".into(),
            StepAction::Perceive,
            StepPayload::Perceive {
                user_query: "hello".into(),
                memory_context: vec![],
            },
        )
        .with_memory_context(MemoryContext {
            retrieved_memory_ids: vec!["mem-1".into()],
            domain_hints: vec!["github".into()],
            boost_terms: vec!["pr".into()],
            provenance: vec!["mem-1".into()],
            ..Default::default()
        })
        .with_timeout_ms(60_000);

        assert!(step.execution.memory_context.is_some());
        let mc = step.execution.memory_context.as_ref().unwrap();
        assert_eq!(mc.retrieved_memory_ids, vec!["mem-1"]);
        assert_eq!(mc.domain_hints, vec!["github"]);
        assert_eq!(step.descriptor.scheduling.timeout_ms, 60_000);
    }

    #[test]
    fn step_lifecycle_started_completed() {
        let mut step = Step::new(
            "s1".into(),
            "t1".into(),
            "n1".into(),
            StepAction::Act,
            StepPayload::Act {
                selected_tools: vec!["grep".into()],
                tool_calls: vec![],
            },
        );
        step.mark_started("agent-01");
        assert_eq!(step.status(), StepStatus::Running);
        assert_eq!(step.descriptor.agent_id.as_deref(), Some("agent-01"));
        assert!(step.execution.started_at.is_some());

        step.mark_completed(StepResult::Act {
            tool_results_count: 1,
            assistant_text: Some("found it".into()),
            tokens_in: 100,
            tokens_out: 50,
        });
        assert!(step.is_terminal());
        assert!(!step.is_retriable());
        assert!(step.execution.completed_at.is_some());
    }

    #[test]
    fn step_lifecycle_failed_retriable() {
        let mut step = Step::new(
            "s1".into(),
            "t1".into(),
            "n1".into(),
            StepAction::Act,
            StepPayload::Act {
                selected_tools: vec![],
                tool_calls: vec![],
            },
        );
        step.mark_started("agent-01");
        step.mark_failed("timeout");
        assert!(step.is_terminal());
        assert!(step.is_retriable()); // attempt=1, max=3, scheduler decides
    }

    // ── Execution Slots (Cursor v3) ──

    #[test]
    fn cursor_default_perceive() {
        let cursor = ExecutionCursor::default();
        assert_eq!(cursor.phase, StepAction::Perceive);
        assert!(cursor.slots.is_empty());
        assert!(cursor.all_slots_done()); // vacuously true
    }

    #[test]
    fn cursor_act_with_slots() {
        let mut cursor = ExecutionCursor::for_act(3);
        assert_eq!(cursor.phase, StepAction::Act);
        assert_eq!(cursor.slots.len(), 3);
        assert_eq!(cursor.pending_slot_count(), 3);
        assert_eq!(cursor.completed_slot_count(), 0);
        assert!(!cursor.all_slots_done());

        // Complete first slot
        cursor.slots[0].tool_name = "grep".into();
        cursor.advance_slot(0, SlotState::Completed);
        assert_eq!(cursor.completed_slot_count(), 1);
        assert_eq!(cursor.pending_slot_count(), 2);
        assert_eq!(cursor.next_pending_slot(), Some(1));

        // Complete second slot
        cursor.slots[1].tool_name = "read_file".into();
        cursor.advance_slot(1, SlotState::Completed);
        assert_eq!(cursor.next_pending_slot(), Some(2));

        // Skip third slot
        cursor.advance_slot(2, SlotState::Skipped);
        assert!(cursor.all_slots_done());
        assert_eq!(cursor.completed_slot_count(), 2);
        assert!(cursor.next_pending_slot().is_none());
    }

    #[test]
    fn cursor_failed_slot_still_done() {
        let mut cursor = ExecutionCursor::for_act(2);
        cursor.advance_slot(0, SlotState::Completed);
        cursor.advance_slot(1, SlotState::Failed);
        assert!(cursor.all_slots_done());
        assert_eq!(cursor.failed_slot_count(), 1);
    }

    #[test]
    fn cursor_parallel_act() {
        let cursor = ExecutionCursor::for_parallel_act(3);
        assert!(cursor.parallel);
        assert_eq!(cursor.slots.len(), 3);
        assert_eq!(cursor.pending_slot_count(), 3);
    }

    #[test]
    fn cursor_parallel_independent_slots() {
        let mut cursor = ExecutionCursor::for_parallel_act(3);
        // In parallel mode, slots are independent — complete any in any order
        cursor.advance_slot(2, SlotState::Completed);
        cursor.advance_slot(0, SlotState::Completed);
        assert_eq!(cursor.completed_slot_count(), 2);
        assert_eq!(cursor.pending_slot_count(), 1);
        assert!(!cursor.all_slots_done());

        cursor.advance_slot(1, SlotState::Failed);
        assert!(cursor.all_slots_done());
    }

    // ── Wait Trigger ──

    #[test]
    fn cursor_wait_with_trigger() {
        let trigger = WaitTrigger {
            trigger_type: WaitTriggerType::Webhook,
            continuation_token: "https://hooks.example.com/callback/12345".into(),
            timeout_ms: Some(300_000),
        };
        let cursor = ExecutionCursor::for_wait(trigger);
        assert_eq!(cursor.phase, StepAction::Wait);
        assert!(cursor.wait_trigger.is_some());
        let wt = cursor.wait_trigger.as_ref().unwrap();
        assert_eq!(wt.trigger_type, WaitTriggerType::Webhook);
        assert!(wt.continuation_token.contains("12345"));
        assert_eq!(wt.timeout_ms, Some(300_000));
    }

    #[test]
    fn cursor_wait_user_trigger() {
        let trigger = WaitTrigger {
            trigger_type: WaitTriggerType::User,
            continuation_token: "prompt-uuid-abc".into(),
            timeout_ms: None, // wait indefinitely
        };
        let cursor = ExecutionCursor::for_wait(trigger);
        let wt = cursor.wait_trigger.unwrap();
        assert_eq!(wt.trigger_type, WaitTriggerType::User);
        assert!(wt.timeout_ms.is_none());
    }

    #[test]
    fn wait_trigger_serde_roundtrip() {
        let trigger = WaitTrigger {
            trigger_type: WaitTriggerType::Timer,
            continuation_token: "timer-30s".into(),
            timeout_ms: Some(30_000),
        };
        let json = serde_json::to_string(&trigger).unwrap();
        let restored: WaitTrigger = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.trigger_type, WaitTriggerType::Timer);
        assert_eq!(restored.continuation_token, "timer-30s");
    }

    // ── Tiered Checkpoints ──

    #[test]
    fn checkpoint_light_creation() {
        let cursor = ExecutionCursor::for_act(2);
        let cp = StepCheckpoint::light("s1".into(), "t1".into(), "a1".into(), cursor);
        assert!(!cp.is_heavy());
        assert_eq!(cp.protocol_version(), PROTOCOL_VERSION);
        assert!(cp.validate().is_ok());
    }

    #[test]
    fn checkpoint_heavy_creation() {
        let cursor = ExecutionCursor::for_act(2);
        let mut cp = StepCheckpoint::heavy("s1".into(), "t1".into(), "a1".into(), cursor);
        assert!(cp.is_heavy());
        assert_eq!(cp.protocol_version(), PROTOCOL_VERSION);
        // Heavy checkpoint for non-Perceive phase needs messages to pass validation
        if let StepCheckpoint::Heavy(ref mut h) = cp {
            h.messages = vec![serde_json::json!({"role": "user", "content": "test"})];
        }
        assert!(cp.validate().is_ok());
    }

    #[test]
    fn checkpoint_new_creates_heavy() {
        let cursor = ExecutionCursor::for_act(1);
        let cp = StepCheckpoint::new("s1".into(), "t1".into(), "a1".into(), cursor);
        assert!(cp.is_heavy()); // backward compat default
    }

    #[test]
    fn checkpoint_wrong_version_rejected() {
        let cursor = ExecutionCursor::for_act(1);
        let cp = match StepCheckpoint::new("s1".into(), "t1".into(), "a1".into(), cursor) {
            StepCheckpoint::Heavy(mut h) => {
                h.light.protocol_version = 999;
                StepCheckpoint::Heavy(h)
            }
            _ => unreachable!(),
        };
        assert!(cp.validate().is_err());
    }

    #[test]
    fn checkpoint_act_without_slots_rejected() {
        let cursor = ExecutionCursor {
            phase: StepAction::Act,
            slots: vec![], // Invalid: ACT must have slots
            parallel: false,
            wait_trigger: None,
            sub_step: None,
        };
        let cp = StepCheckpoint::light("s1".into(), "t1".into(), "a1".into(), cursor);
        let err = cp.validate().unwrap_err();
        assert!(matches!(err, ProtocolError::InvalidCursor(_)));
    }

    #[test]
    fn checkpoint_serde_roundtrip() {
        let mut cursor = ExecutionCursor::for_act(2);
        cursor.slots[0] = ExecutionSlot {
            index: 0,
            tool_name: "grep".into(),
            call_id: "c1".into(),
            state: SlotState::Completed,
            idempotency_key: Some("key1".into()),
            args_preview: Some("pattern=foo".into()),
            cached_result: Some(CachedToolResult {
                tool_name: "grep".into(),
                output: "3 matches".into(),
                is_error: false,
                cached_at: 1000,
            }),
            retry_count: 0,
        };
        cursor.slots[1] = ExecutionSlot {
            index: 1,
            tool_name: "bash".into(),
            call_id: "c2".into(),
            state: SlotState::Running,
            idempotency_key: None,
            args_preview: None,
            cached_result: None,
            retry_count: 1,
        };
        let cp = StepCheckpoint::heavy("s1".into(), "t1".into(), "a1".into(), cursor);
        if let StepCheckpoint::Heavy(ref h) = cp {
            // Verify Heavy fields accessible
            assert!(h.messages.is_empty());
        }
        let json = serde_json::to_string(&cp).unwrap();
        let restored: StepCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.protocol_version(), PROTOCOL_VERSION);
        assert_eq!(restored.cursor().slots.len(), 2);
        assert_eq!(restored.cursor().slots[0].state, SlotState::Completed);
        assert_eq!(restored.cursor().slots[1].state, SlotState::Running);
        assert!(restored.cursor().slots[0].cached_result.is_some());
    }

    // ── Idempotency ──

    #[test]
    fn idempotency_key_deterministic() {
        let args = serde_json::json!({"path": "/src/main.rs", "line": 42});
        let k1 = IdempotencyKey::new("step-1", 0, "read_file", &args);
        let k2 = IdempotencyKey::new("step-1", 0, "read_file", &args);
        assert_eq!(k1.content_hash, k2.content_hash);
        assert_eq!(k1.cache_key(), k2.cache_key());
    }

    #[test]
    fn idempotency_key_different_tools_different_hash() {
        let args = serde_json::json!({"path": "/src/main.rs"});
        let k1 = IdempotencyKey::new("step-1", 0, "read_file", &args);
        let k2 = IdempotencyKey::new("step-1", 0, "write_file", &args);
        assert_ne!(k1.content_hash, k2.content_hash);
    }

    #[test]
    fn idempotency_key_different_args_different_hash() {
        let k1 = IdempotencyKey::new("step-1", 0, "grep", &serde_json::json!({"pattern": "foo"}));
        let k2 = IdempotencyKey::new("step-1", 0, "grep", &serde_json::json!({"pattern": "bar"}));
        assert_ne!(k1.content_hash, k2.content_hash);
    }

    #[test]
    fn idempotency_key_semantic_vs_execution() {
        let args = serde_json::json!({"query": "hello world"});
        let exec_key = IdempotencyKey::new("step-1", 0, "grep", &args);
        let sem_key = IdempotencyKey::semantic("grep", &args);
        assert!(!exec_key.is_semantic());
        assert!(exec_key.cache_key().starts_with("step-1:"));
        assert!(sem_key.is_semantic());
        assert!(sem_key.cache_key().starts_with("sem:"));
        assert_eq!(exec_key.content_hash, sem_key.content_hash);
    }

    #[test]
    fn idempotency_key_semantic_diff_step_same_content() {
        let args = serde_json::json!({"path": "src/main.rs"});
        let key_a = IdempotencyKey::new("step-A", 0, "read_file", &args);
        let key_b = IdempotencyKey::new("step-B", 0, "read_file", &args);
        let key_s = IdempotencyKey::semantic("read_file", &args);
        assert_ne!(key_a.cache_key(), key_b.cache_key());
        assert_eq!(key_a.content_hash, key_b.content_hash);
        assert_eq!(key_a.content_hash, key_s.content_hash);
    }

    #[test]
    fn idempotency_key_with_context_signature() {
        let args = serde_json::json!({"path": "src/main.rs"});
        let key_no_ctx = IdempotencyKey::new("s1", 0, "read_file", &args);
        let key_with_ctx =
            IdempotencyKey::new("s1", 0, "read_file", &args).with_context(ContextSignature {
                workspace_version: Some("abc12345deadbeef".into()),
                memory_snapshot_id: None,
            });

        // Same content hash, different cache keys
        assert_eq!(key_no_ctx.content_hash, key_with_ctx.content_hash);
        assert_ne!(key_no_ctx.cache_key(), key_with_ctx.cache_key());
        assert!(key_with_ctx.cache_key().contains(":ws=abc12345"));
    }

    #[test]
    fn idempotency_key_context_memory_snapshot() {
        let args = serde_json::json!({});
        let key = IdempotencyKey::semantic("memory_search", &args).with_context(ContextSignature {
            workspace_version: None,
            memory_snapshot_id: Some("snap-20250101".into()),
        });
        assert!(key.cache_key().contains(":ms=snap-202"));
    }

    #[test]
    fn inmemory_cache_basic_operations() {
        let mut cache = InMemoryIdempotencyCache::new();
        assert!(cache.is_empty());

        let key = IdempotencyKey::new("s1", 0, "grep", &serde_json::json!({}));
        assert!(cache.check(&key).is_none());

        cache.record(
            &key,
            CachedToolResult {
                tool_name: "grep".into(),
                output: "3 matches".into(),
                is_error: false,
                cached_at: epoch_ms(),
            },
        );
        assert_eq!(cache.len(), 1);
        assert!(cache.check(&key).is_some());
        assert_eq!(cache.check(&key).unwrap().output, "3 matches");
    }

    #[test]
    fn inmemory_cache_evict_step() {
        let mut cache = InMemoryIdempotencyCache::new();
        let k1 = IdempotencyKey::new("step-A", 0, "grep", &serde_json::json!({}));
        let k2 = IdempotencyKey::new("step-A", 1, "read_file", &serde_json::json!({}));
        let k3 = IdempotencyKey::new("step-B", 0, "grep", &serde_json::json!({}));
        for key in [&k1, &k2, &k3] {
            cache.record(
                key,
                CachedToolResult {
                    tool_name: "t".into(),
                    output: "r".into(),
                    is_error: false,
                    cached_at: 0,
                },
            );
        }
        assert_eq!(cache.len(), 3);
        cache.evict_step("step-A");
        assert_eq!(cache.len(), 1);
        assert!(cache.check(&k1).is_none());
        assert!(cache.check(&k3).is_some());
    }

    // ── Tool Idempotency Classification ──

    #[test]
    fn tool_classification_read_tools() {
        for tool in [
            "read_file",
            "grep",
            "glob",
            "list_dir",
            "git_status",
            "git_log",
            "git_diff",
            "git_blame",
            "github_list_prs",
            "github_ci_status",
            "mo_query",
            "memory_search",
        ] {
            assert_eq!(
                classify_tool_idempotency(tool),
                ToolIdempotency::PureRead,
                "Expected PureRead for {tool}"
            );
        }
    }

    #[test]
    fn tool_classification_idempotent_write() {
        assert_eq!(
            classify_tool_idempotency("write_file"),
            ToolIdempotency::IdempotentWrite
        );
    }

    #[test]
    fn tool_classification_non_idempotent() {
        for tool in [
            "bash",
            "str_replace",
            "github_create_issue",
            "memory_store",
            "memory_purge",
            "mo_snapshot",
        ] {
            assert_eq!(
                classify_tool_idempotency(tool),
                ToolIdempotency::NonIdempotent,
                "Expected NonIdempotent for {tool}"
            );
        }
    }

    #[test]
    fn unknown_tool_defaults_to_non_idempotent() {
        assert_eq!(
            classify_tool_idempotency("some_future_tool"),
            ToolIdempotency::NonIdempotent
        );
    }

    // ── Retry Policy ──

    #[test]
    fn retry_policy_backoff_exponential() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.backoff_ms(0), 500);
        assert_eq!(policy.backoff_ms(1), 1000);
        assert_eq!(policy.backoff_ms(2), 2000);
        assert_eq!(policy.backoff_ms(3), 4000);
    }

    #[test]
    fn retry_policy_backoff_capped() {
        let policy = RetryPolicy {
            backoff_max_ms: 5000,
            ..RetryPolicy::default()
        };
        assert_eq!(policy.backoff_ms(10), 5000);
    }

    #[test]
    fn retry_policy_should_retry() {
        let policy = RetryPolicy::default();
        assert!(policy.should_retry(1, &ErrorCategory::Transient));
        assert!(policy.should_retry(2, &ErrorCategory::Timeout));
        assert!(!policy.should_retry(3, &ErrorCategory::Transient)); // max_attempts=3
        assert!(!policy.should_retry(1, &ErrorCategory::AuthFailure)); // not in retry_on
    }

    #[test]
    fn retry_policy_max_retries_caps_max_attempts() {
        let policy = RetryPolicy {
            max_attempts: 100,
            max_retries: 5,
            ..RetryPolicy::default()
        };
        assert!(policy.should_retry(0, &ErrorCategory::Transient));
        assert!(policy.should_retry(4, &ErrorCategory::Transient));
        assert!(!policy.should_retry(5, &ErrorCategory::Transient));
    }

    // ── Tool Retry Policy ──

    #[test]
    fn tool_retry_policy_pure_read() {
        let policy = tool_retry_policy("grep");
        assert_eq!(policy.max_attempts, 3);
        assert_eq!(policy.backoff_base_ms, 200);
    }

    #[test]
    fn tool_retry_policy_idempotent_write() {
        let policy = tool_retry_policy("write_file");
        assert_eq!(policy.max_attempts, 2);
        assert_eq!(policy.backoff_base_ms, 500);
    }

    #[test]
    fn tool_retry_policy_non_idempotent() {
        let policy = tool_retry_policy("bash");
        assert_eq!(policy.max_attempts, 1);
    }

    // ── Canonical JSON ──

    #[test]
    fn canonical_json_sorted_keys() {
        let args_a = serde_json::json!({"z": 1, "a": 2, "m": 3});
        let args_b = serde_json::json!({"a": 2, "m": 3, "z": 1});
        let key_a = IdempotencyKey::semantic("tool", &args_a);
        let key_b = IdempotencyKey::semantic("tool", &args_b);
        assert_eq!(key_a.content_hash, key_b.content_hash);
    }

    #[test]
    fn canonical_json_nested_sorted() {
        let args_a = serde_json::json!({"outer": {"z": 1, "a": 2}});
        let args_b = serde_json::json!({"outer": {"a": 2, "z": 1}});
        let key_a = IdempotencyKey::semantic("tool", &args_a);
        let key_b = IdempotencyKey::semantic("tool", &args_b);
        assert_eq!(key_a.content_hash, key_b.content_hash);
    }

    #[test]
    fn in_memory_cache_evicts_by_tool_name() {
        let mut cache = InMemoryIdempotencyCache::new();
        let read_key = IdempotencyKey::semantic("read_file", &serde_json::json!({"path": "a.rs"}));
        let grep_key = IdempotencyKey::semantic("grep", &serde_json::json!({"pattern": "foo"}));

        cache.record(
            &read_key,
            CachedToolResult {
                tool_name: "read_file".into(),
                output: "old file".into(),
                is_error: false,
                cached_at: 0,
            },
        );
        cache.record(
            &grep_key,
            CachedToolResult {
                tool_name: "grep".into(),
                output: "old grep".into(),
                is_error: false,
                cached_at: 0,
            },
        );

        cache.evict_tool("read_file");

        assert!(cache.check(&read_key).is_none());
        assert!(cache.check(&grep_key).is_some());
    }

    // ── Execution Slot with Cached Result ──

    #[test]
    fn execution_slot_with_cached_result() {
        let slot = ExecutionSlot {
            index: 0,
            tool_name: "grep".into(),
            call_id: "c1".into(),
            state: SlotState::Completed,
            idempotency_key: Some("key1".into()),
            args_preview: Some("pattern=foo".into()),
            cached_result: Some(CachedToolResult {
                tool_name: "grep".into(),
                output: "3 matches".into(),
                is_error: false,
                cached_at: 1000,
            }),
            retry_count: 0,
        };
        let json = serde_json::to_string(&slot).unwrap();
        let restored: ExecutionSlot = serde_json::from_str(&json).unwrap();
        assert!(restored.cached_result.is_some());
        assert_eq!(restored.cached_result.unwrap().output, "3 matches");
    }

    // ── Event Store DAG traversal (via FileBackedEventStore) ──

    #[test]
    fn event_store_trait_append_and_len() {
        use crate::step_checkpoint::FileBackedEventStore;
        let mut store = FileBackedEventStore::empty("test-trait");
        <FileBackedEventStore as StepEventStore>::append(
            &mut store,
            StepEvent {
                event_id: "e1".into(),
                step_id: "s1".into(),
                event_type: StepEventType::StepStarted,
                agent_id: None,
                caused_by: vec![],
                payload: None,
                created_at: 100,
            },
        );
        assert_eq!(<FileBackedEventStore as StepEventStore>::len(&store), 1);
    }

    #[test]
    fn event_store_events_for_step() {
        use crate::step_checkpoint::FileBackedEventStore;
        let mut store = FileBackedEventStore::empty("test-events-for-step");
        store.append(StepEvent {
            event_id: "e1".into(),
            step_id: "s1".into(),
            event_type: StepEventType::StepStarted,
            agent_id: None,
            caused_by: vec![],
            payload: None,
            created_at: 100,
        });
        store.append(StepEvent {
            event_id: "e2".into(),
            step_id: "s2".into(),
            event_type: StepEventType::StepStarted,
            agent_id: None,
            caused_by: vec![],
            payload: None,
            created_at: 200,
        });
        store.append(StepEvent {
            event_id: "e3".into(),
            step_id: "s1".into(),
            event_type: StepEventType::StepCompleted,
            agent_id: None,
            caused_by: vec!["e1".into()],
            payload: None,
            created_at: 300,
        });
        let s1_events = store.events_for_step("s1");
        assert_eq!(s1_events.len(), 2);
        let s2_events = store.events_for_step("s2");
        assert_eq!(s2_events.len(), 1);
    }

    #[test]
    fn event_store_single_parent_chain() {
        use crate::step_checkpoint::FileBackedEventStore;
        let mut store = FileBackedEventStore::empty("test-chain");
        store.append(StepEvent {
            event_id: "e1".into(),
            step_id: "s1".into(),
            event_type: StepEventType::StepStarted,
            agent_id: None,
            caused_by: vec![],
            payload: None,
            created_at: 100,
        });
        store.append(StepEvent {
            event_id: "e2".into(),
            step_id: "s1".into(),
            event_type: StepEventType::ToolCallStarted,
            agent_id: None,
            caused_by: vec!["e1".into()],
            payload: None,
            created_at: 200,
        });
        store.append(StepEvent {
            event_id: "e3".into(),
            step_id: "s1".into(),
            event_type: StepEventType::ToolCallCompleted,
            agent_id: None,
            caused_by: vec!["e2".into()],
            payload: None,
            created_at: 300,
        });
        assert_eq!(store.len(), 3);
        let leaves = store.leaves();
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].event_id, "e3");
        let ancestors = store.ancestors("e3");
        assert_eq!(ancestors.len(), 2);
        let desc = store.descendants("e1");
        assert_eq!(desc.len(), 2);
    }

    #[test]
    fn event_store_multi_parent_convergence() {
        use crate::step_checkpoint::FileBackedEventStore;
        let mut store = FileBackedEventStore::empty("test-convergence");
        store.append(StepEvent {
            event_id: "start".into(),
            step_id: "s1".into(),
            event_type: StepEventType::StepStarted,
            agent_id: None,
            caused_by: vec![],
            payload: None,
            created_at: 100,
        });
        for (i, tool) in ["grep", "read_file", "git_log"].iter().enumerate() {
            store.append(StepEvent {
                event_id: format!("tool_start_{i}"),
                step_id: "s1".into(),
                event_type: StepEventType::ToolCallStarted,
                agent_id: None,
                caused_by: vec!["start".into()],
                payload: Some(serde_json::json!({"tool": tool})),
                created_at: 200 + i as u64,
            });
        }
        for i in 0..3 {
            store.append(StepEvent {
                event_id: format!("tool_done_{i}"),
                step_id: "s1".into(),
                event_type: StepEventType::ToolCallCompleted,
                agent_id: None,
                caused_by: vec![format!("tool_start_{i}")],
                payload: None,
                created_at: 400 + i as u64,
            });
        }
        store.append(StepEvent {
            event_id: "converge".into(),
            step_id: "s1".into(),
            event_type: StepEventType::ToolsConverged,
            agent_id: None,
            caused_by: vec![
                "tool_done_0".into(),
                "tool_done_1".into(),
                "tool_done_2".into(),
            ],
            payload: None,
            created_at: 500,
        });
        assert_eq!(store.len(), 8);
        let leaves = store.leaves();
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].event_id, "converge");
        let ancestors = store.ancestors("converge");
        assert_eq!(ancestors.len(), 7);
        let desc = store.descendants("start");
        assert_eq!(desc.len(), 7);
    }

    #[test]
    fn event_store_empty() {
        use crate::step_checkpoint::FileBackedEventStore;
        let store = FileBackedEventStore::empty("test-empty");
        assert!(store.is_empty());
        assert!(store.leaves().is_empty());
    }

    // ── Step Action Display ──

    #[test]
    fn step_action_display() {
        assert_eq!(StepAction::Perceive.to_string(), "PERCEIVE");
        assert_eq!(StepAction::Act.to_string(), "ACT");
        assert_eq!(StepAction::Done.to_string(), "DONE");
        assert!(StepAction::Done.is_terminal());
        assert!(StepAction::Fail.is_terminal());
        assert!(!StepAction::Act.is_terminal());
    }

    // ── Step Serde Roundtrip ──

    #[test]
    fn step_serde_roundtrip() {
        let step = Step::new(
            "step-001".into(),
            "task-001".into(),
            "node-a".into(),
            StepAction::Act,
            StepPayload::Act {
                selected_tools: vec!["grep".into(), "bash".into()],
                tool_calls: vec![serde_json::json!({"id": "tc1", "name": "grep"})],
            },
        );
        let json = serde_json::to_string(&step).unwrap();
        let restored: Step = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.step_id(), "step-001");
        assert_eq!(restored.descriptor.protocol_version, PROTOCOL_VERSION);
        assert_eq!(restored.action(), StepAction::Act);
    }

    // ── StepResult variants ──

    #[test]
    fn step_result_variants_serialize() {
        let results = vec![
            StepResult::Perceive {
                intent_signals: vec!["is_code_review".into()],
                intent_confidence: 0.85,
                entities: vec!["main.rs".into()],
                memory_matches: 3,
                boost_terms: vec!["code".into(), "review".into()],
            },
            StepResult::Plan {
                selected_tools: vec!["grep".into()],
                confidence: 0.9,
            },
            StepResult::Evaluate {
                verdict: StepVerdict::Continue,
                progress: 0.5,
                should_continue: true,
                next_action: StepAction::Act,
            },
            StepResult::Error {
                message: "tool timeout".into(),
            },
        ];
        for result in &results {
            let json = serde_json::to_string(result).unwrap();
            let _restored: StepResult = serde_json::from_str(&json).unwrap();
        }
    }

    // ── Memory Context ──

    #[test]
    fn memory_context_default_empty() {
        let mc = MemoryContext::default();
        assert!(mc.retrieved_memory_ids.is_empty());
        assert!(mc.domain_hints.is_empty());
        assert!(mc.boost_terms.is_empty());
        assert!(mc.provenance.is_empty());
    }

    #[test]
    fn memory_context_serde_roundtrip() {
        let mc = MemoryContext {
            retrieved_memory_ids: vec!["mem-1".into(), "mem-2".into()],
            domain_hints: vec!["github".into()],
            boost_terms: vec!["pr".into(), "review".into()],
            provenance: vec!["mem-1".into()],
            ..Default::default()
        };
        let json = serde_json::to_string(&mc).unwrap();
        let restored: MemoryContext = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.retrieved_memory_ids, mc.retrieved_memory_ids);
        assert_eq!(restored.domain_hints, mc.domain_hints);
    }

    // ── Step Accessor Methods ──

    #[test]
    fn step_accessor_methods() {
        let step = Step::new(
            "s1".into(),
            "t1".into(),
            "n1".into(),
            StepAction::Act,
            StepPayload::Act {
                selected_tools: vec!["grep".into()],
                tool_calls: vec![],
            },
        );
        assert_eq!(step.step_id(), "s1");
        assert_eq!(step.action(), StepAction::Act);
        assert_eq!(step.status(), StepStatus::Pending);
    }

    // ── Memory Governance ──

    #[test]
    fn memory_governance_action_variants() {
        let actions = vec![
            MemoryGovernanceAction::Retrieved {
                memory_id: "m1".into(),
            },
            MemoryGovernanceAction::Promoted {
                memory_id: "m2".into(),
                reason: "confirmed".into(),
            },
            MemoryGovernanceAction::Purged {
                memory_id: "m3".into(),
                reason: "stale".into(),
            },
            MemoryGovernanceAction::Corrected {
                memory_id: "m4".into(),
                reason: "updated".into(),
            },
            MemoryGovernanceAction::ClusterAnalyzed { cluster_count: 5 },
            MemoryGovernanceAction::Reflected {
                summary: "session productive".into(),
            },
        ];
        for action in &actions {
            let json = serde_json::to_string(action).unwrap();
            let restored: MemoryGovernanceAction = serde_json::from_str(&json).unwrap();
            assert_eq!(&restored, action);
        }
    }

    #[test]
    fn memory_context_governance_fields() {
        let mc = MemoryContext {
            retrieved_memory_ids: vec!["m1".into()],
            domain_hints: vec![],
            boost_terms: vec![],
            provenance: vec![],
            governance_actions: vec![
                MemoryGovernanceAction::Retrieved {
                    memory_id: "m1".into(),
                },
                MemoryGovernanceAction::Promoted {
                    memory_id: "m1".into(),
                    reason: "useful".into(),
                },
            ],
            cluster_insights: vec!["3 clusters found".into()],
            snapshot_id: Some("snap-001".into()),
        };
        let json = serde_json::to_string(&mc).unwrap();
        let restored: MemoryContext = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.governance_actions.len(), 2);
        assert_eq!(restored.cluster_insights.len(), 1);
        assert_eq!(restored.snapshot_id.as_deref(), Some("snap-001"));
    }

    // ── IdempotencyCache Trait ──

    #[test]
    fn idempotency_cache_trait_inmemory() {
        let mut cache: Box<dyn IdempotencyCache> = Box::new(InMemoryIdempotencyCache::new());
        let key = IdempotencyKey::new("s1", 0, "grep", &serde_json::json!({"pattern": "test"}));
        assert!(cache.check(&key).is_none());
        cache.record(
            &key,
            CachedToolResult {
                tool_name: "grep".into(),
                output: "found".into(),
                is_error: false,
                cached_at: 0,
            },
        );
        assert!(cache.check(&key).is_some());
        cache.evict_step("s1");
        assert!(cache.check(&key).is_none());
    }

    // ── Checkpoint Trigger Strategy ──

    #[test]
    fn checkpoint_trigger_tier_mapping() {
        assert_eq!(
            CheckpointTrigger::SlotCompleted.checkpoint_tier(),
            CheckpointTier::Light
        );
        assert_eq!(
            CheckpointTrigger::BeforeExpensiveOp.checkpoint_tier(),
            CheckpointTier::Light
        );
        assert_eq!(
            CheckpointTrigger::PhaseTransition.checkpoint_tier(),
            CheckpointTier::Heavy
        );
        assert_eq!(
            CheckpointTrigger::Explicit.checkpoint_tier(),
            CheckpointTier::Heavy
        );
    }

    #[test]
    fn checkpoint_trigger_serde_roundtrip() {
        let triggers = [
            CheckpointTrigger::SlotCompleted,
            CheckpointTrigger::PhaseTransition,
            CheckpointTrigger::BeforeExpensiveOp,
            CheckpointTrigger::Explicit,
        ];
        for t in &triggers {
            let json = serde_json::to_string(t).unwrap();
            let restored: CheckpointTrigger = serde_json::from_str(&json).unwrap();
            assert_eq!(&restored, t);
        }
    }

    // ── Canonical JSON Consistency ──

    #[test]
    fn compute_idempotency_key_canonical() {
        // Same content, different key order → same idempotency key
        let key1 = compute_idempotency_key(
            "t1",
            "n1",
            &StepAction::Act,
            &StepPayload::Act {
                selected_tools: vec!["grep".into()],
                tool_calls: vec![serde_json::json!({"a": 1, "b": 2})],
            },
        );
        let key2 = compute_idempotency_key(
            "t1",
            "n1",
            &StepAction::Act,
            &StepPayload::Act {
                selected_tools: vec!["grep".into()],
                tool_calls: vec![serde_json::json!({"b": 2, "a": 1})],
            },
        );
        // With canonical JSON, these should produce same hash
        assert_eq!(key1, key2);
    }

    // ── Migration Registry ──

    #[test]
    fn migration_registry_basic() {
        let mut reg = MigrationRegistry::new();
        assert!(!reg.has_migration(0));

        fn v0_to_v1(_ver: u32, data: &serde_json::Value) -> Result<serde_json::Value, String> {
            let mut obj = data.clone();
            if let Some(map) = obj.as_object_mut() {
                map.insert("protocol_version".into(), serde_json::json!(1));
            }
            Ok(obj)
        }

        reg.register(0, v0_to_v1);
        assert!(reg.has_migration(0));
        assert!(!reg.has_migration(99));

        let old_data = serde_json::json!({"cursor": "test"});
        let migrated = reg.migrate(0, &old_data).unwrap();
        assert_eq!(migrated["protocol_version"], 1);
        assert_eq!(migrated["cursor"], "test");
    }

    #[test]
    fn migration_registry_missing_version() {
        let reg = MigrationRegistry::new();
        let result = reg.migrate(42, &serde_json::json!({}));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("42"));
    }

    #[test]
    fn migration_registry_with_defaults_has_v0() {
        let reg = MigrationRegistry::with_defaults();
        assert!(
            reg.has_migration(0),
            "v0→v1000 migration must be registered"
        );
        assert!(!reg.has_migration(1), "no v1 migration expected");
        assert!(!reg.has_migration(999), "no v999 migration expected");
    }

    #[test]
    fn migrate_v0_adds_protocol_version() {
        let reg = MigrationRegistry::with_defaults();
        let legacy = serde_json::json!({
            "cursor": {"current_step": 0, "slots": []},
            "turn_state": {"turn": 3}
        });
        let migrated = reg.migrate(0, &legacy).unwrap();
        assert_eq!(migrated["protocol_version"], PROTOCOL_VERSION);
        // Original fields preserved
        assert_eq!(migrated["cursor"]["current_step"], 0);
        assert_eq!(migrated["turn_state"]["turn"], 3);
    }

    #[test]
    fn migrate_v0_preserves_existing_version_field() {
        let reg = MigrationRegistry::with_defaults();
        let already_versioned = serde_json::json!({
            "protocol_version": 500,
            "cursor": {"current_step": 0}
        });
        let migrated = reg.migrate(0, &already_versioned).unwrap();
        // Does NOT overwrite existing protocol_version
        assert_eq!(migrated["protocol_version"], 500);
    }

    #[test]
    fn migrate_v0_heavy_checkpoint_adds_to_inner_light() {
        let reg = MigrationRegistry::with_defaults();
        let heavy = serde_json::json!({
            "light": {
                "cursor": {"current_step": 2},
                "turn_state": {"turn": 5}
            },
            "full_conversation": []
        });
        let migrated = reg.migrate(0, &heavy).unwrap();
        // Top level gets protocol_version
        assert_eq!(migrated["protocol_version"], PROTOCOL_VERSION);
        // Inner light also gets it
        assert_eq!(migrated["light"]["protocol_version"], PROTOCOL_VERSION);
    }

    #[test]
    fn migrate_non_object_returns_error() {
        let reg = MigrationRegistry::with_defaults();
        let result = reg.migrate(0, &serde_json::json!("not an object"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not a JSON object"));
    }

    // ── Version Display per Policy ──

    #[test]
    fn version_display_strict_says_discard() {
        let err = ProtocolError::VersionMismatch {
            expected: 1000,
            found: 999,
            policy: VersionPolicy::Strict,
        };
        assert!(err.to_string().contains("Discard checkpoint and restart"));
    }

    #[test]
    fn version_display_compatible_says_incompatible() {
        let err = ProtocolError::VersionMismatch {
            expected: 1000,
            found: 2000,
            policy: VersionPolicy::Compatible,
        };
        assert!(err.to_string().contains("Incompatible major version"));
    }

    #[test]
    fn version_display_migrate_says_no_path() {
        let err = ProtocolError::VersionMismatch {
            expected: 2000,
            found: 50,
            policy: VersionPolicy::Migrate,
        };
        assert!(err.to_string().contains("No migration path"));
    }

    // ── Recovery Boundary: validate() hardened ──

    #[test]
    fn validate_wait_without_trigger_rejects() {
        let cp = StepCheckpoint::Light(LightCheckpoint {
            protocol_version: PROTOCOL_VERSION,
            cursor: ExecutionCursor {
                phase: StepAction::Wait,
                slots: vec![],
                parallel: false,
                wait_trigger: None, // missing!
                sub_step: None,
            },
            step_id: "s1".into(),
            task_id: "t1".into(),
            agent_id: "a1".into(),
            progress: 0.0,
            total_tokens: 0,
            created_at: 0,
        });
        let err = cp.validate().unwrap_err();
        assert!(matches!(err, ProtocolError::InvalidCursor(_)));
        assert!(err.to_string().contains("wait_trigger"));
    }

    #[test]
    fn validate_running_slot_in_checkpoint_rejects() {
        let cp = StepCheckpoint::Light(LightCheckpoint {
            protocol_version: PROTOCOL_VERSION,
            cursor: ExecutionCursor {
                phase: StepAction::Act,
                slots: vec![ExecutionSlot {
                    index: 0,
                    tool_name: "grep".into(),
                    call_id: "c1".into(),
                    state: SlotState::Running, // crash artifact!
                    idempotency_key: None,
                    args_preview: None,
                    cached_result: None,
                    retry_count: 0,
                }],
                parallel: false,
                wait_trigger: None,
                sub_step: None,
            },
            step_id: "s1".into(),
            task_id: "t1".into(),
            agent_id: "a1".into(),
            progress: 0.5,
            total_tokens: 100,
            created_at: 0,
        });
        let err = cp.validate().unwrap_err();
        assert!(matches!(err, ProtocolError::CheckpointCorrupt(_)));
        assert!(err.to_string().contains("Running state"));
    }

    #[test]
    fn validate_heavy_no_messages_for_act_rejects() {
        let cp = StepCheckpoint::Heavy(Box::new(HeavyCheckpoint {
            light: LightCheckpoint {
                protocol_version: PROTOCOL_VERSION,
                cursor: ExecutionCursor::for_act(2),
                step_id: "s1".into(),
                task_id: "t1".into(),
                agent_id: "a1".into(),
                progress: 0.5,
                total_tokens: 500,
                created_at: 0,
            },
            messages: vec![], // empty for non-Perceive!
            budget_remaining_tokens: 1000,
            budget_remaining_rounds: 5,
            blocked_tools: vec![],
            recent_tools: vec![],
            learning_snapshot_id: None,
            memory_context: None,
            delegation_id: None,
            delegation_pattern: None,
            delegation_sub_run_summaries: Vec::new(),
            interruption: None,
            approval_overrides: None,
            consecutive_context_window_errors: 0,
            compaction_state: None,
            pipeline_state: None,
            continuity_state: None,
        }));
        let err = cp.validate().unwrap_err();
        assert!(matches!(err, ProtocolError::CheckpointCorrupt(_)));
        assert!(err.to_string().contains("no messages"));
    }

    #[test]
    fn validate_heavy_perceive_no_messages_ok() {
        // Perceive phase is allowed to have no messages (initial state)
        let cp = StepCheckpoint::Heavy(Box::new(HeavyCheckpoint {
            light: LightCheckpoint {
                protocol_version: PROTOCOL_VERSION,
                cursor: ExecutionCursor::default(), // Perceive
                step_id: "s1".into(),
                task_id: "t1".into(),
                agent_id: "a1".into(),
                progress: 0.0,
                total_tokens: 0,
                created_at: 0,
            },
            messages: vec![],
            budget_remaining_tokens: 4000,
            budget_remaining_rounds: 10,
            blocked_tools: vec![],
            recent_tools: vec![],
            learning_snapshot_id: None,
            memory_context: None,
            delegation_id: None,
            delegation_pattern: None,
            delegation_sub_run_summaries: Vec::new(),
            interruption: None,
            approval_overrides: None,
            consecutive_context_window_errors: 0,
            compaction_state: None,
            pipeline_state: None,
            continuity_state: None,
        }));
        assert!(cp.validate().is_ok());
    }

    #[test]
    fn validate_completed_slots_ok() {
        let cp = StepCheckpoint::Light(LightCheckpoint {
            protocol_version: PROTOCOL_VERSION,
            cursor: ExecutionCursor {
                phase: StepAction::Act,
                slots: vec![
                    ExecutionSlot {
                        index: 0,
                        tool_name: "grep".into(),
                        call_id: "c1".into(),
                        state: SlotState::Completed,
                        idempotency_key: None,
                        args_preview: None,
                        cached_result: None,
                        retry_count: 0,
                    },
                    ExecutionSlot {
                        index: 1,
                        tool_name: "read_file".into(),
                        call_id: "c2".into(),
                        state: SlotState::Completed,
                        idempotency_key: None,
                        args_preview: None,
                        cached_result: None,
                        retry_count: 0,
                    },
                ],
                parallel: false,
                wait_trigger: None,
                sub_step: None,
            },
            step_id: "s1".into(),
            task_id: "t1".into(),
            agent_id: "a1".into(),
            progress: 1.0,
            total_tokens: 200,
            created_at: 0,
        });
        assert!(cp.validate().is_ok());
    }

    // ── Checkpoint Round-Trip ──

    #[test]
    fn checkpoint_light_serde_roundtrip() {
        let cp = StepCheckpoint::light(
            "s1".into(),
            "t1".into(),
            "a1".into(),
            ExecutionCursor::for_act(2),
        );
        let json = serde_json::to_string(&cp).unwrap();
        let restored: StepCheckpoint = serde_json::from_str(&json).unwrap();
        assert!(!restored.is_heavy());
        assert_eq!(restored.protocol_version(), PROTOCOL_VERSION);
        assert_eq!(restored.cursor().slots.len(), 2);
    }

    #[test]
    fn checkpoint_heavy_serde_roundtrip() {
        let mut cp = StepCheckpoint::heavy(
            "s1".into(),
            "t1".into(),
            "a1".into(),
            ExecutionCursor::default(),
        );
        if let StepCheckpoint::Heavy(ref mut h) = cp {
            h.messages = vec![serde_json::json!({"role": "user", "content": "hello"})];
            h.budget_remaining_tokens = 2000;
            h.blocked_tools = vec!["bash".into()];
            h.learning_snapshot_id = Some("snap-001".into());
        }
        let json = serde_json::to_string(&cp).unwrap();
        let restored: StepCheckpoint = serde_json::from_str(&json).unwrap();
        assert!(restored.is_heavy());
        if let StepCheckpoint::Heavy(h) = restored {
            assert_eq!(h.messages.len(), 1);
            assert_eq!(h.budget_remaining_tokens, 2000);
            assert_eq!(h.blocked_tools, vec!["bash"]);
            assert_eq!(h.learning_snapshot_id.as_deref(), Some("snap-001"));
        }
    }

    // ── Eviction Safety ──

    #[test]
    fn evict_step_no_prefix_collision() {
        let mut cache = InMemoryIdempotencyCache::new();
        // "s1:0:abc" and "s10:0:def" — evicting "s1" must NOT touch "s10"
        let key_s1 = IdempotencyKey::new("s1", 0, "grep", &serde_json::json!({"a": 1}));
        let key_s10 = IdempotencyKey::new("s10", 0, "grep", &serde_json::json!({"a": 1}));
        let result = CachedToolResult {
            tool_name: "grep".into(),
            output: "ok".into(),
            is_error: false,
            cached_at: 0,
        };
        cache.record(&key_s1, result.clone());
        cache.record(&key_s10, result);
        assert_eq!(cache.len(), 2);

        cache.evict_step("s1");
        assert_eq!(cache.len(), 1);
        // s10 should still be there
        assert!(cache.check(&key_s10).is_some());
        // s1 should be gone
        assert!(cache.check(&key_s1).is_none());
    }

    // ── Canonical JSON Key Escaping ──

    #[test]
    fn canonical_json_escapes_special_keys() {
        let val = serde_json::json!({"key\"with\"quotes": 1, "normal": 2});
        let result = canonical_json(&val);
        // Keys must be properly escaped — should contain escaped quotes
        assert!(result.contains(r#"key\"with\"quotes"#));
        // Should be valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["normal"], 2);
    }

    // ── Version Scheme Sanity ──

    #[test]
    fn version_scheme_major_minor_encoding() {
        assert_eq!(PROTOCOL_VERSION, 1000);
        assert_eq!(PROTOCOL_VERSION_MAJOR, 1);
        assert_eq!(PROTOCOL_VERSION_MINOR, 0);
        assert_eq!(PROTOCOL_VERSION / 1000, PROTOCOL_VERSION_MAJOR);
        assert_eq!(PROTOCOL_VERSION % 1000, PROTOCOL_VERSION_MINOR);
    }

    #[test]
    fn version_migrate_too_old_rejects() {
        // major 0 → ok (N-1 migration). But if current major were 3, major 0 would be too old.
        // Simulate: pretend expected major=1, found major=0 → ok for N-1
        let result = check_protocol_version_with_policy(50, VersionPolicy::Migrate);
        assert!(result.is_ok()); // major 0 is N-1 of major 1

        // But version in a completely different range (future major 5) is rejected
        let result = check_protocol_version_with_policy(5000, VersionPolicy::Migrate);
        assert!(result.is_err()); // major 5 is not same nor N-1
    }

    #[test]
    fn version_compatible_rejects_old_versions() {
        // Old version in major 0 range, current is major 1
        let result = check_protocol_version_with_policy(500, VersionPolicy::Compatible);
        assert!(result.is_err()); // major 0 != major 1
    }

    // ── Wait Trigger Validation ──

    #[test]
    fn validate_wait_with_trigger_ok() {
        let cp = StepCheckpoint::Light(LightCheckpoint {
            protocol_version: PROTOCOL_VERSION,
            cursor: ExecutionCursor {
                phase: StepAction::Wait,
                slots: vec![],
                parallel: false,
                wait_trigger: Some(WaitTrigger {
                    trigger_type: WaitTriggerType::User,
                    continuation_token: "tok-1".into(),
                    timeout_ms: None,
                }),
                sub_step: None,
            },
            step_id: "s1".into(),
            task_id: "t1".into(),
            agent_id: "a1".into(),
            progress: 0.0,
            total_tokens: 0,
            created_at: 0,
        });
        assert!(cp.validate().is_ok());
    }

    // ── SchedulingContract tests ─────────────────────────────────────────

    #[test]
    fn scheduling_contract_defaults() {
        let c = SchedulingContract::default();
        assert_eq!(c.priority, 5);
        assert_eq!(c.timeout_ms, 300_000);
        assert_eq!(c.per_tool_timeout_ms, 0);
        assert_eq!(c.max_retries, 2);
        assert_eq!(c.backoff_base_ms, 500);
        assert_eq!(c.backoff_max_ms, 5_000);
    }

    #[test]
    fn scheduling_contract_backoff_exponential() {
        let c = SchedulingContract::default();
        assert_eq!(c.backoff_ms(0), 500); // 500 * 2^0
        assert_eq!(c.backoff_ms(1), 1000); // 500 * 2^1
        assert_eq!(c.backoff_ms(2), 2000); // 500 * 2^2
        assert_eq!(c.backoff_ms(3), 4000); // 500 * 2^3
        assert_eq!(c.backoff_ms(4), 5000); // capped at max
    }

    #[test]
    fn scheduling_contract_effective_tool_timeout() {
        let c = SchedulingContract::default(); // 300s step, 0 per-tool
        // With 3 tools: 300_000 / 3 = 100_000ms per tool (above 30s floor)
        assert_eq!(c.effective_tool_timeout_ms(3), 100_000);
        // With 1 tool: full step timeout
        assert_eq!(c.effective_tool_timeout_ms(1), 300_000);
        // With 0 tools: full step timeout (edge case)
        assert_eq!(c.effective_tool_timeout_ms(0), 300_000);

        // Explicit per-tool timeout overrides
        let c2 = SchedulingContract {
            per_tool_timeout_ms: 30_000,
            ..Default::default()
        };
        assert_eq!(c2.effective_tool_timeout_ms(3), 30_000);
        assert_eq!(c2.effective_tool_timeout_ms(1), 30_000);

        // Floor: many tools should not starve individual tools below 30s
        let c3 = SchedulingContract {
            timeout_ms: 60_000, // 60s step
            ..Default::default()
        };
        // 60_000 / 5 = 12_000 which is below floor → clamp to 30_000
        assert_eq!(c3.effective_tool_timeout_ms(5), 30_000);
        // 60_000 / 2 = 30_000 which equals floor → OK
        assert_eq!(c3.effective_tool_timeout_ms(2), 30_000);
        // 60_000 / 1 = 60_000 which is above floor → unchanged
        assert_eq!(c3.effective_tool_timeout_ms(1), 60_000);
    }

    #[test]
    fn scheduling_contract_serde_roundtrip() {
        let c = SchedulingContract {
            priority: 8,
            timeout_ms: 60_000,
            per_tool_timeout_ms: 10_000,
            max_retries: 5,
            backoff_base_ms: 200,
            backoff_max_ms: 10_000,
        };
        let json = serde_json::to_string(&c).unwrap();
        let c2: SchedulingContract = serde_json::from_str(&json).unwrap();
        assert_eq!(c2.priority, 8);
        assert_eq!(c2.timeout_ms, 60_000);
        assert_eq!(c2.max_retries, 5);
    }

    #[test]
    fn step_with_scheduling_contract() {
        let step = Step::new(
            "step-1".into(),
            "task-1".into(),
            "node-1".into(),
            StepAction::Act,
            StepPayload::Act {
                selected_tools: vec![],
                tool_calls: vec![],
            },
        )
        .with_scheduling(SchedulingContract {
            priority: 10,
            timeout_ms: 60_000,
            ..Default::default()
        });
        assert_eq!(step.descriptor.scheduling.priority, 10);
        assert_eq!(step.descriptor.scheduling.timeout_ms, 60_000);
        assert_eq!(step.descriptor.scheduling.max_retries, 2); // default
    }

    #[test]
    fn step_backward_compat_with_timeout() {
        // with_timeout_ms still works (sets scheduling.timeout_ms)
        let step = Step::new(
            "step-1".into(),
            "task-1".into(),
            "node-1".into(),
            StepAction::Perceive,
            StepPayload::Perceive {
                user_query: "test".into(),
                memory_context: vec![],
            },
        )
        .with_timeout_ms(120_000);
        assert_eq!(step.descriptor.scheduling.timeout_ms, 120_000);
    }

    fn make_heavy_with_continuity(cs: Option<serde_json::Value>) -> HeavyCheckpoint {
        HeavyCheckpoint {
            light: LightCheckpoint {
                protocol_version: PROTOCOL_VERSION,
                cursor: ExecutionCursor::default(),
                step_id: "s".into(),
                task_id: "t".into(),
                agent_id: "a".into(),
                progress: 0.0,
                total_tokens: 0,
                created_at: 0,
            },
            messages: vec![],
            budget_remaining_tokens: 0,
            budget_remaining_rounds: 0,
            blocked_tools: vec![],
            recent_tools: vec![],
            learning_snapshot_id: None,
            memory_context: None,
            delegation_id: None,
            delegation_pattern: None,
            delegation_sub_run_summaries: vec![],
            interruption: None,
            approval_overrides: None,
            consecutive_context_window_errors: 0,
            compaction_state: None,
            pipeline_state: None,
            continuity_state: cs,
        }
    }

    #[test]
    fn heavy_checkpoint_rejects_malformed_continuity_state_via_validator() {
        let cp = make_heavy_with_continuity(Some(serde_json::json!({"todos": "not-an-object"})));
        let result = cp.validate_with(|v| {
            v.get("todos")
                .and_then(|t| t.as_object())
                .ok_or_else(|| "todos must be object".to_string())?;
            Ok(())
        });
        assert!(matches!(
            result,
            Err(ValidationError::ContinuityStateSchema(_))
        ));
    }

    #[test]
    fn heavy_checkpoint_accepts_valid_continuity_state_via_validator() {
        let cp = make_heavy_with_continuity(Some(serde_json::json!({"todos": {}})));
        let result = cp.validate_with(|v| {
            v.get("todos")
                .and_then(|t| t.as_object())
                .ok_or_else(|| "todos must be object".to_string())?;
            Ok(())
        });
        assert!(result.is_ok());
    }

    #[test]
    fn heavy_checkpoint_validate_with_passes_when_no_continuity_state() {
        let cp = make_heavy_with_continuity(None);
        let result = cp.validate_with(|_| Err("should not be called".to_string()));
        assert!(result.is_ok());
    }
}
