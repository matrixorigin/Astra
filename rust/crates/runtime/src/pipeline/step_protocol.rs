//! Step Protocol v2: Layered, idempotent, cursor-aware execution unit.
//!
//! # Architecture: 3 Concerns, 3 Types
//!
//! ```text
//! ┌─ StepDescriptor ──────────────┐  Scheduling layer (who/when/retry)
//! │  step_id, task_id, action,    │  Immutable after creation
//! │  scheduling, retry_policy     │
//! ├─ StepExecution ───────────────┤  Runtime layer (cursor/progress)
//! │  cursor, tool_completions,    │  Mutable during execution
//! │  result, memory_context       │
//! ├─ StepCheckpoint ──────────────┤  Persistence layer (recoverable state)
//! │  version, cursor, messages,   │  Written to storage at safe points
//! │  budget, tool_results_cache   │
//! └───────────────────────────────┘
//! ```
//!
//! # Key properties
//!
//! - **Versioned**: `protocol_version` with `VersionPolicy` (Strict/BestEffort).
//! - **Idempotent**: Tool-level `IdempotencyKey` with semantic dedup option.
//! - **Cursor-aware**: `ExecutionCursor` supports parallel tool slots.
//! - **Tool-level retry**: `ToolRetryPolicy` per tool, not per step.
//! - **Memory-integrated**: `MemoryContext` flows through step lifecycle.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─── Protocol Version ────────────────────────────────────────────────────────

/// Current protocol version. Embedded in every Step and Checkpoint.
pub const PROTOCOL_VERSION: u32 = 1;

/// How to handle version mismatches on checkpoint restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VersionPolicy {
    /// Reject any mismatch (safe default for production)
    Strict,
    /// Accept if major version matches (minor bumps OK).
    /// major = version / 100, minor = version % 100.
    /// v1xx can restore from v1xx but not v2xx.
    BestEffort,
}

impl Default for VersionPolicy {
    fn default() -> Self {
        Self::Strict
    }
}

/// Check protocol version compatibility.
/// With BestEffort, allows same major version (e.g., v101 can restore v100).
pub fn check_protocol_version(version: u32) -> Result<(), ProtocolError> {
    check_protocol_version_with_policy(version, VersionPolicy::Strict)
}

pub fn check_protocol_version_with_policy(
    version: u32,
    policy: VersionPolicy,
) -> Result<(), ProtocolError> {
    // Version 0 is always invalid regardless of policy
    if version == 0 {
        return Err(ProtocolError::VersionMismatch {
            expected: PROTOCOL_VERSION,
            found: 0,
            policy,
        });
    }
    match policy {
        VersionPolicy::Strict => {
            if version != PROTOCOL_VERSION {
                return Err(ProtocolError::VersionMismatch {
                    expected: PROTOCOL_VERSION,
                    found: version,
                    policy,
                });
            }
        }
        VersionPolicy::BestEffort => {
            let expected_major = PROTOCOL_VERSION / 100;
            let found_major = version / 100;
            if expected_major != found_major {
                return Err(ProtocolError::VersionMismatch {
                    expected: PROTOCOL_VERSION,
                    found: version,
                    policy,
                });
            }
        }
    }
    Ok(())
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
                write!(
                    f,
                    "Protocol version mismatch: expected v{expected}, found v{found} \
                     (policy: {policy:?}). Discard checkpoint and restart."
                )
            }
            Self::InvalidCursor(msg) => write!(f, "Invalid execution cursor: {msg}"),
            Self::CheckpointCorrupt(msg) => write!(f, "Corrupt checkpoint: {msg}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

// ─── Step: Layered Structure ─────────────────────────────────────────────────

/// Scheduling descriptor (immutable after creation, owned by Scheduler).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepDescriptor {
    pub step_id: String,
    pub task_id: String,
    pub dag_node_id: String,
    pub parent_step_id: Option<String>,
    pub action: StepAction,
    pub agent_id: Option<String>,
    pub timeout_ms: u64,
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
                timeout_ms: 300_000,
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
        self.descriptor.timeout_ms = timeout_ms;
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

// ─── Execution Cursor ────────────────────────────────────────────────────────

/// Precise execution position within a Step.
/// Supports both sequential and parallel tool execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionCursor {
    /// Current phase being executed
    pub phase: StepAction,
    /// Within ACT: which tool call (0-based, for sequential mode)
    pub tool_index: u32,
    /// Per-tool completion tracking (ACT phase only)
    pub tool_completions: Vec<ToolCompletion>,
    /// Execution mode: sequential or parallel
    pub parallel: bool,
    /// For Wait steps: continuation token for async resume
    pub continuation_token: Option<String>,
    /// Sub-step identifier (future: nested/composite steps)
    pub sub_step: Option<String>,
}

impl Default for ExecutionCursor {
    fn default() -> Self {
        Self {
            phase: StepAction::Perceive,
            tool_index: 0,
            tool_completions: Vec::new(),
            parallel: false,
            continuation_token: None,
            sub_step: None,
        }
    }
}

impl ExecutionCursor {
    /// Create cursor for an ACT step with N tool calls (sequential)
    pub fn for_act(num_tools: usize) -> Self {
        Self {
            phase: StepAction::Act,
            tool_index: 0,
            tool_completions: (0..num_tools)
                .map(|_| ToolCompletion {
                    tool_name: String::new(),
                    call_id: String::new(),
                    status: ToolCompletionStatus::Pending,
                    idempotency_key: None,
                    cached_result: None,
                    retry_count: 0,
                })
                .collect(),
            parallel: false,
            continuation_token: None,
            sub_step: None,
        }
    }

    /// Create cursor for parallel tool execution
    pub fn for_parallel_act(num_tools: usize) -> Self {
        let mut cursor = Self::for_act(num_tools);
        cursor.parallel = true;
        cursor
    }

    /// Create cursor for Wait step with continuation token
    pub fn for_wait(token: String) -> Self {
        Self {
            phase: StepAction::Wait,
            continuation_token: Some(token),
            ..Self::default()
        }
    }

    /// Advance cursor after a tool call completes
    pub fn advance_tool(&mut self, index: usize, status: ToolCompletionStatus) {
        if let Some(tc) = self.tool_completions.get_mut(index) {
            tc.status = status;
        }
        // Move to next pending tool
        self.tool_index = self
            .tool_completions
            .iter()
            .position(|tc| tc.status == ToolCompletionStatus::Pending)
            .unwrap_or(self.tool_completions.len() as usize) as u32;
    }

    /// Are all tool calls in ACT completed (or skipped)?
    pub fn all_tools_done(&self) -> bool {
        self.tool_completions.iter().all(|tc| {
            matches!(
                tc.status,
                ToolCompletionStatus::Completed
                    | ToolCompletionStatus::Failed
                    | ToolCompletionStatus::Skipped
            )
        })
    }

    /// Count of completed tool calls
    pub fn completed_tool_count(&self) -> usize {
        self.tool_completions
            .iter()
            .filter(|tc| tc.status == ToolCompletionStatus::Completed)
            .count()
    }

    /// Count of pending tool calls
    pub fn pending_tool_count(&self) -> usize {
        self.tool_completions
            .iter()
            .filter(|tc| tc.status == ToolCompletionStatus::Pending)
            .count()
    }
}

/// Per-tool completion tracking within an ACT step.
/// Includes cached result for crash recovery (no need for separate cache lookup).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCompletion {
    pub tool_name: String,
    pub call_id: String,
    pub status: ToolCompletionStatus,
    /// Points to idempotency cache entry
    pub idempotency_key: Option<String>,
    /// Inline cached result (for checkpoint completeness)
    pub cached_result: Option<CachedToolResult>,
    /// Tool-level retry count (separate from step retry)
    pub retry_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolCompletionStatus {
    /// Not yet executed
    Pending,
    /// Currently executing (crash point)
    Running,
    /// Completed, result in idempotency cache
    Completed,
    /// Execution failed
    Failed,
    /// Skipped (dedup or conditional)
    Skipped,
}

// ─── Checkpoint ──────────────────────────────────────────────────────────────

/// Recoverable snapshot at a point in Step execution.
/// Contains both state AND cursor (the position to resume from).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepCheckpoint {
    /// Protocol version at creation time (for compatibility check)
    pub protocol_version: u32,
    /// Exact execution position
    pub cursor: ExecutionCursor,

    // Conversation state
    pub messages: Vec<serde_json::Value>,
    pub budget_remaining_tokens: u64,
    pub budget_remaining_rounds: u32,
    pub progress: f64,
    pub blocked_tools: Vec<String>,
    pub total_tokens: u64,
    pub recent_tools: Vec<String>,

    // Associations
    pub step_id: String,
    pub task_id: String,
    pub agent_id: String,
    pub learning_snapshot_id: Option<String>,

    // Time
    pub created_at: u64,
}

impl StepCheckpoint {
    pub fn new(step_id: String, task_id: String, agent_id: String, cursor: ExecutionCursor) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            cursor,
            messages: Vec::new(),
            budget_remaining_tokens: 0,
            budget_remaining_rounds: 0,
            progress: 0.0,
            blocked_tools: Vec::new(),
            total_tokens: 0,
            recent_tools: Vec::new(),
            step_id,
            task_id,
            agent_id,
            learning_snapshot_id: None,
            created_at: epoch_ms(),
        }
    }

    /// Validate checkpoint before restoring
    pub fn validate(&self) -> Result<(), ProtocolError> {
        check_protocol_version(self.protocol_version)?;
        // Cursor consistency: if ACT phase, tool_completions should exist
        if self.cursor.phase == StepAction::Act && self.cursor.tool_completions.is_empty() {
            return Err(ProtocolError::InvalidCursor(
                "ACT phase cursor has no tool completions".into(),
            ));
        }
        Ok(())
    }
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

/// Step-level retry policy (fallback when tool-level not specified).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff_base_ms: u64,
    pub backoff_max_ms: u64,
    pub retry_on: Vec<ErrorCategory>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
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
        attempt < self.max_attempts && self.retry_on.contains(category)
    }
}

/// Tool-level retry policy (more granular than step-level).
/// A single tool failure doesn't force whole-step retry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRetryPolicy {
    pub max_attempts: u32,
    pub backoff_base_ms: u64,
    pub backoff_max_ms: u64,
}

impl Default for ToolRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 2,
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
        attempt < self.max_attempts
    }
}

/// Get tool-level retry policy based on idempotency classification.
pub fn tool_retry_policy(tool_name: &str) -> ToolRetryPolicy {
    match classify_tool_idempotency(tool_name) {
        // Pure reads: retry aggressively (no side effects)
        ToolIdempotency::PureRead => ToolRetryPolicy {
            max_attempts: 3,
            backoff_base_ms: 200,
            backoff_max_ms: 2_000,
        },
        // Idempotent writes: retry cautiously
        ToolIdempotency::IdempotentWrite => ToolRetryPolicy {
            max_attempts: 2,
            backoff_base_ms: 500,
            backoff_max_ms: 5_000,
        },
        // Non-idempotent: do NOT auto-retry (let LLM decide)
        ToolIdempotency::NonIdempotent => ToolRetryPolicy {
            max_attempts: 1, // no retry
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
/// Supports both execution-level (step+index) and semantic-level (content-only) lookup.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IdempotencyKey {
    pub step_id: String,
    pub tool_index: u32,
    pub content_hash: String,
}

impl IdempotencyKey {
    /// Execution-level key: tied to specific step + tool index
    pub fn new(step_id: &str, tool_index: u32, tool_name: &str, args: &serde_json::Value) -> Self {
        let content_hash = compute_content_hash(tool_name, args);
        Self {
            step_id: step_id.to_string(),
            tool_index,
            content_hash,
        }
    }

    /// Semantic-level key: content-only (for DAG reuse / step replay)
    pub fn semantic(tool_name: &str, args: &serde_json::Value) -> Self {
        let content_hash = compute_content_hash(tool_name, args);
        Self {
            step_id: String::new(), // empty = semantic key
            tool_index: 0,
            content_hash,
        }
    }

    /// Execution-level cache key (step-specific)
    pub fn cache_key(&self) -> String {
        if self.step_id.is_empty() {
            format!("sem:{}", self.content_hash)
        } else {
            format!("{}:{}:{}", self.step_id, self.tool_index, self.content_hash)
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

/// Tool idempotency classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolIdempotency {
    /// Safe to re-execute (no side effects): read_file, grep, git_log, etc.
    PureRead,
    /// Overwrite-style write (safe if file unchanged): write_file
    IdempotentWrite,
    /// Must check cache, never blindly re-execute: bash, github_create_issue
    NonIdempotent,
}

/// Classify a tool's idempotency level.
pub fn classify_tool_idempotency(tool_name: &str) -> ToolIdempotency {
    match tool_name {
        // Pure read tools — safe to re-execute
        "read_file" | "grep" | "glob" | "list_dir" | "git_status" | "git_log" | "git_diff"
        | "git_blame" | "git_file_history" | "git_contributors" | "git_log_search"
        | "github_list_prs" | "github_get_pr" | "github_list_issues" | "github_get_issue"
        | "github_ci_status" | "github_repo_stats" | "mo_query" | "memory_search"
        | "memory_profile" | "web_fetch" | "get_agent_info" | "reflect" => ToolIdempotency::PureRead,

        // Idempotent writes — overwrite semantics
        "write_file" => ToolIdempotency::IdempotentWrite,

        // Non-idempotent — must cache result
        "bash" | "str_replace" | "github_create_issue" | "memory_store" | "memory_purge"
        | "memory_correct" | "mo_snapshot" | "mo_branch" => ToolIdempotency::NonIdempotent,

        // Unknown tools: treat as non-idempotent (safe default)
        _ => ToolIdempotency::NonIdempotent,
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

    /// Remove all entries for a step (cleanup after step completes)
    pub fn evict_step(&mut self, step_id: &str) {
        self.cache.retain(|k, _| !k.starts_with(step_id));
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}

// ─── Step Event (DAG) ────────────────────────────────────────────────────────

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepEventType {
    StepCreated,
    StepAssigned,
    StepStarted,
    StepCompleted,
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

    StallDetected,
    DivergenceDetected,
    RetryScheduled,
}

/// DAG of step events with multi-parent traversal.
#[derive(Debug, Default)]
pub struct StepEventDag {
    events: Vec<StepEvent>,
}

impl StepEventDag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, event: StepEvent) {
        self.events.push(event);
    }

    pub fn events(&self) -> &[StepEvent] {
        &self.events
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Find all ancestors (BFS up the caused_by DAG).
    pub fn ancestors(&self, event_id: &str) -> Vec<&StepEvent> {
        let mut result = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let mut visited = std::collections::HashSet::new();
        queue.push_back(event_id.to_string());
        visited.insert(event_id.to_string());

        while let Some(current) = queue.pop_front() {
            if let Some(ev) = self.events.iter().find(|e| e.event_id == current) {
                if ev.event_id != event_id {
                    result.push(ev);
                }
                for parent in &ev.caused_by {
                    if visited.insert(parent.clone()) {
                        queue.push_back(parent.clone());
                    }
                }
            }
        }
        result
    }

    /// Find all descendants (BFS down from event_id).
    pub fn descendants(&self, event_id: &str) -> Vec<&StepEvent> {
        let mut result = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let mut visited = std::collections::HashSet::new();
        queue.push_back(event_id.to_string());
        visited.insert(event_id.to_string());

        while let Some(current) = queue.pop_front() {
            for ev in &self.events {
                if ev.caused_by.contains(&current) && visited.insert(ev.event_id.clone()) {
                    result.push(ev);
                    queue.push_back(ev.event_id.clone());
                }
            }
        }
        result
    }

    /// Find leaf events (no children).
    pub fn leaves(&self) -> Vec<&StepEvent> {
        let parents: std::collections::HashSet<&str> = self
            .events
            .iter()
            .flat_map(|e| e.caused_by.iter().map(|s| s.as_str()))
            .collect();
        self.events
            .iter()
            .filter(|e| !parents.contains(e.event_id.as_str()))
            .collect()
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn epoch_ms() -> u64 {
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
    format!("{:x}", hash)[..16].to_string()
}

/// Produce canonical JSON with sorted keys (recursively).
fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            sorted.sort_by_key(|(k, _)| *k);
            let entries: Vec<String> = sorted
                .iter()
                .map(|(k, v)| format!("\"{}\":{}", k, canonical_json(v)))
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
    let payload_json = serde_json::to_string(payload).unwrap_or_default();
    hasher.update(payload_json.as_bytes());
    let hash = hasher.finalize();
    format!("{:x}", hash)[..32].to_string() // 32-char prefix
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
        // Error message should mention "Discard"
        assert!(err.to_string().contains("Discard"));
    }

    #[test]
    fn protocol_version_zero_rejected() {
        assert!(check_protocol_version(0).is_err());
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
        })
        .with_timeout_ms(60_000);

        assert!(step.execution.memory_context.is_some());
        let mc = step.execution.memory_context.as_ref().unwrap();
        assert_eq!(mc.retrieved_memory_ids, vec!["mem-1"]);
        assert_eq!(mc.domain_hints, vec!["github"]);
        assert_eq!(step.descriptor.timeout_ms, 60_000);
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
        // attempt=1, max=3, status=Failed (not Completed/Cancelled)
        // so is_retriable = true (scheduler decides whether to actually retry)
        assert!(step.is_retriable());
    }

    // ── Execution Cursor ──

    #[test]
    fn cursor_default_perceive() {
        let cursor = ExecutionCursor::default();
        assert_eq!(cursor.phase, StepAction::Perceive);
        assert_eq!(cursor.tool_index, 0);
        assert!(cursor.tool_completions.is_empty());
        assert!(cursor.all_tools_done()); // vacuously true
    }

    #[test]
    fn cursor_act_with_tools() {
        let mut cursor = ExecutionCursor::for_act(3);
        assert_eq!(cursor.phase, StepAction::Act);
        assert_eq!(cursor.tool_completions.len(), 3);
        assert_eq!(cursor.pending_tool_count(), 3);
        assert_eq!(cursor.completed_tool_count(), 0);
        assert!(!cursor.all_tools_done());

        // Complete first tool
        cursor.tool_completions[0].tool_name = "grep".into();
        cursor.advance_tool(0, ToolCompletionStatus::Completed);
        assert_eq!(cursor.tool_index, 1);
        assert_eq!(cursor.completed_tool_count(), 1);
        assert_eq!(cursor.pending_tool_count(), 2);

        // Complete second tool
        cursor.tool_completions[1].tool_name = "read_file".into();
        cursor.advance_tool(1, ToolCompletionStatus::Completed);
        assert_eq!(cursor.tool_index, 2);

        // Skip third tool
        cursor.advance_tool(2, ToolCompletionStatus::Skipped);
        assert!(cursor.all_tools_done());
        assert_eq!(cursor.completed_tool_count(), 2);
    }

    #[test]
    fn cursor_failed_tool_still_done() {
        let mut cursor = ExecutionCursor::for_act(2);
        cursor.advance_tool(0, ToolCompletionStatus::Completed);
        cursor.advance_tool(1, ToolCompletionStatus::Failed);
        assert!(cursor.all_tools_done());
    }

    // ── Checkpoint ──

    #[test]
    fn checkpoint_creation_and_validation() {
        let cursor = ExecutionCursor::for_act(2);
        let cp = StepCheckpoint::new("s1".into(), "t1".into(), "a1".into(), cursor);
        assert_eq!(cp.protocol_version, PROTOCOL_VERSION);
        assert!(cp.validate().is_ok());
    }

    #[test]
    fn checkpoint_wrong_version_rejected() {
        let cursor = ExecutionCursor::for_act(1);
        let mut cp = StepCheckpoint::new("s1".into(), "t1".into(), "a1".into(), cursor);
        cp.protocol_version = 999;
        assert!(cp.validate().is_err());
    }

    #[test]
    fn checkpoint_act_without_tools_rejected() {
        let cursor = ExecutionCursor {
            phase: StepAction::Act,
            tool_index: 0,
            tool_completions: vec![], // Invalid: ACT must have tools
            parallel: false,
            continuation_token: None,
            sub_step: None,
        };
        let cp = StepCheckpoint::new("s1".into(), "t1".into(), "a1".into(), cursor);
        let err = cp.validate().unwrap_err();
        assert!(matches!(err, ProtocolError::InvalidCursor(_)));
    }

    #[test]
    fn checkpoint_serde_roundtrip() {
        let mut cursor = ExecutionCursor::for_act(2);
        cursor.tool_completions[0] = ToolCompletion {
            tool_name: "grep".into(),
            call_id: "c1".into(),
            status: ToolCompletionStatus::Completed,
            idempotency_key: Some("key1".into()),
            cached_result: Some(CachedToolResult {
                tool_name: "grep".into(),
                output: "3 matches".into(),
                is_error: false,
                cached_at: 1000,
            }),
            retry_count: 0,
        };
        cursor.tool_completions[1] = ToolCompletion {
            tool_name: "bash".into(),
            call_id: "c2".into(),
            status: ToolCompletionStatus::Running,
            idempotency_key: None,
            cached_result: None,
            retry_count: 1,
        };
        let mut cp = StepCheckpoint::new("s1".into(), "t1".into(), "a1".into(), cursor);
        cp.messages = vec![serde_json::json!({"role": "user", "content": "hello"})];
        cp.total_tokens = 500;
        cp.progress = 0.6;
        cp.blocked_tools = vec!["dangerous_tool".into()];

        let json = serde_json::to_string(&cp).unwrap();
        let restored: StepCheckpoint = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.protocol_version, PROTOCOL_VERSION);
        assert_eq!(restored.cursor.tool_completions.len(), 2);
        assert_eq!(
            restored.cursor.tool_completions[0].status,
            ToolCompletionStatus::Completed
        );
        assert_eq!(
            restored.cursor.tool_completions[1].status,
            ToolCompletionStatus::Running
        );
        assert_eq!(restored.total_tokens, 500);
        assert_eq!(restored.progress, 0.6);
        assert_eq!(restored.blocked_tools, vec!["dangerous_tool"]);
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
        assert!(cache.check(&k2).is_none());
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
        assert_eq!(policy.backoff_ms(0), 500); // 500 * 2^0
        assert_eq!(policy.backoff_ms(1), 1000); // 500 * 2^1
        assert_eq!(policy.backoff_ms(2), 2000); // 500 * 2^2
        assert_eq!(policy.backoff_ms(3), 4000);
    }

    #[test]
    fn retry_policy_backoff_capped() {
        let policy = RetryPolicy {
            backoff_max_ms: 5000,
            ..RetryPolicy::default()
        };
        assert_eq!(policy.backoff_ms(10), 5000); // capped
    }

    #[test]
    fn retry_policy_should_retry() {
        let policy = RetryPolicy::default();
        assert!(policy.should_retry(1, &ErrorCategory::Transient));
        assert!(policy.should_retry(2, &ErrorCategory::Timeout));
        assert!(!policy.should_retry(3, &ErrorCategory::Transient)); // max_attempts=3
        assert!(!policy.should_retry(1, &ErrorCategory::AuthFailure)); // not in retry_on
    }

    // ── Step Event DAG ──

    #[test]
    fn event_dag_single_parent_chain() {
        let mut dag = StepEventDag::new();
        dag.push(StepEvent {
            event_id: "e1".into(),
            step_id: "s1".into(),
            event_type: StepEventType::StepStarted,
            agent_id: None,
            caused_by: vec![],
            payload: None,
            created_at: 100,
        });
        dag.push(StepEvent {
            event_id: "e2".into(),
            step_id: "s1".into(),
            event_type: StepEventType::ToolCallStarted,
            agent_id: None,
            caused_by: vec!["e1".into()],
            payload: None,
            created_at: 200,
        });
        dag.push(StepEvent {
            event_id: "e3".into(),
            step_id: "s1".into(),
            event_type: StepEventType::ToolCallCompleted,
            agent_id: None,
            caused_by: vec!["e2".into()],
            payload: None,
            created_at: 300,
        });

        assert_eq!(dag.len(), 3);
        let leaves = dag.leaves();
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].event_id, "e3");

        let ancestors = dag.ancestors("e3");
        assert_eq!(ancestors.len(), 2); // e1, e2
        let desc = dag.descendants("e1");
        assert_eq!(desc.len(), 2); // e2, e3
    }

    #[test]
    fn event_dag_multi_parent_convergence() {
        let mut dag = StepEventDag::new();
        // Parallel tool calls converging
        dag.push(StepEvent {
            event_id: "start".into(),
            step_id: "s1".into(),
            event_type: StepEventType::StepStarted,
            agent_id: None,
            caused_by: vec![],
            payload: None,
            created_at: 100,
        });
        // Three parallel tool starts
        for (i, tool) in ["grep", "read_file", "git_log"].iter().enumerate() {
            dag.push(StepEvent {
                event_id: format!("tool_start_{i}"),
                step_id: "s1".into(),
                event_type: StepEventType::ToolCallStarted,
                agent_id: None,
                caused_by: vec!["start".into()],
                payload: Some(serde_json::json!({"tool": tool})),
                created_at: 200 + i as u64,
            });
        }
        // Three parallel tool completions
        for i in 0..3 {
            dag.push(StepEvent {
                event_id: format!("tool_done_{i}"),
                step_id: "s1".into(),
                event_type: StepEventType::ToolCallCompleted,
                agent_id: None,
                caused_by: vec![format!("tool_start_{i}")],
                payload: None,
                created_at: 400 + i as u64,
            });
        }
        // Convergence: all three complete → ToolsConverged (multi-parent!)
        dag.push(StepEvent {
            event_id: "converge".into(),
            step_id: "s1".into(),
            event_type: StepEventType::ToolsConverged,
            agent_id: None,
            caused_by: vec!["tool_done_0".into(), "tool_done_1".into(), "tool_done_2".into()],
            payload: None,
            created_at: 500,
        });

        assert_eq!(dag.len(), 8);
        let leaves = dag.leaves();
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].event_id, "converge");

        // Ancestors of converge: tool_done_0..2, tool_start_0..2, start = 7
        let ancestors = dag.ancestors("converge");
        assert_eq!(ancestors.len(), 7);

        // Descendants of start: everything else = 7
        let desc = dag.descendants("start");
        assert_eq!(desc.len(), 7);
    }

    #[test]
    fn event_dag_empty() {
        let dag = StepEventDag::new();
        assert!(dag.is_empty());
        assert!(dag.leaves().is_empty());
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
                selected_tools: vec!["grep".into(), "read_file".into()],
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

    // ── Version Policy ──

    #[test]
    fn version_policy_strict_rejects_mismatch() {
        let result = check_protocol_version_with_policy(2, VersionPolicy::Strict);
        assert!(result.is_err());
    }

    #[test]
    fn version_policy_strict_accepts_exact() {
        let result = check_protocol_version_with_policy(PROTOCOL_VERSION, VersionPolicy::Strict);
        assert!(result.is_ok());
    }

    #[test]
    fn version_policy_besteffort_same_major() {
        // PROTOCOL_VERSION = 1, major = 1/100 = 0
        // version 50 → major = 50/100 = 0 (same major)
        let result = check_protocol_version_with_policy(50, VersionPolicy::BestEffort);
        assert!(result.is_ok());
    }

    #[test]
    fn version_policy_besteffort_diff_major_rejects() {
        // version 100 → major = 100/100 = 1 (different major)
        let result = check_protocol_version_with_policy(100, VersionPolicy::BestEffort);
        assert!(result.is_err());
        if let Err(ProtocolError::VersionMismatch { policy, .. }) = result {
            assert_eq!(policy, VersionPolicy::BestEffort);
        } else {
            panic!("expected VersionMismatch");
        }
    }

    #[test]
    fn version_policy_besteffort_zero_rejected() {
        let result = check_protocol_version_with_policy(0, VersionPolicy::BestEffort);
        assert!(result.is_err());
    }

    // ── Parallel Cursor ──

    #[test]
    fn cursor_parallel_act() {
        let cursor = ExecutionCursor::for_parallel_act(3);
        assert!(cursor.parallel);
        assert_eq!(cursor.tool_completions.len(), 3);
        assert_eq!(cursor.pending_tool_count(), 3);
    }

    #[test]
    fn cursor_parallel_index_stays_zero() {
        let mut cursor = ExecutionCursor::for_parallel_act(3);
        // In parallel mode, all tools dispatched simultaneously
        // Index doesn't advance like sequential
        cursor.advance_tool(1, ToolCompletionStatus::Completed);
        // tool_index still 0 since advance_tool only sets 0→1 for sequential index < slot
        // But the tool IS marked done
        assert_eq!(cursor.completed_tool_count(), 1);
        assert_eq!(cursor.pending_tool_count(), 2);
    }

    #[test]
    fn cursor_wait_continuation_token() {
        let mut cursor = ExecutionCursor::for_act(1);
        cursor.phase = StepAction::Wait;
        cursor.continuation_token = Some("webhook-callback-12345".into());
        assert_eq!(cursor.continuation_token.as_deref(), Some("webhook-callback-12345"));

        let json = serde_json::to_string(&cursor).unwrap();
        let restored: ExecutionCursor = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.continuation_token.as_deref(), Some("webhook-callback-12345"));
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
        // No retry for non-idempotent tools
    }

    // ── Semantic Idempotency Keys ──

    #[test]
    fn idempotency_key_semantic_vs_execution() {
        let args = serde_json::json!({"query": "hello world"});
        let exec_key = IdempotencyKey::new("step-1", 0, "grep", &args);
        let sem_key = IdempotencyKey::semantic("grep", &args);

        // Execution key includes step_id
        assert!(!exec_key.is_semantic());
        assert!(exec_key.cache_key().starts_with("step-1:"));

        // Semantic key is content-only (for DAG reuse)
        assert!(sem_key.is_semantic());
        assert!(sem_key.cache_key().starts_with("sem:"));

        // Same content → same content_hash
        assert_eq!(exec_key.content_hash, sem_key.content_hash);
    }

    #[test]
    fn idempotency_key_semantic_diff_step_same_content() {
        let args = serde_json::json!({"path": "src/main.rs"});
        let key_a = IdempotencyKey::new("step-A", 0, "read_file", &args);
        let key_b = IdempotencyKey::new("step-B", 0, "read_file", &args);
        let key_s = IdempotencyKey::semantic("read_file", &args);

        // Different step_id → different execution keys
        assert_ne!(key_a.cache_key(), key_b.cache_key());
        // Same semantic key
        assert_eq!(key_a.content_hash, key_b.content_hash);
        assert_eq!(key_a.content_hash, key_s.content_hash);
    }

    // ── Canonical JSON ──

    #[test]
    fn canonical_json_sorted_keys() {
        // Two objects with same keys in different order → same hash
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

    // ── Tool Completion with Cached Result ──

    #[test]
    fn tool_completion_with_cached_result() {
        let tc = ToolCompletion {
            tool_name: "grep".into(),
            call_id: "c1".into(),
            status: ToolCompletionStatus::Completed,
            idempotency_key: Some("key1".into()),
            cached_result: Some(CachedToolResult {
                tool_name: "grep".into(),
                output: "3 matches".into(),
                is_error: false,
                cached_at: 1000,
            }),
            retry_count: 0,
        };

        let json = serde_json::to_string(&tc).unwrap();
        let restored: ToolCompletion = serde_json::from_str(&json).unwrap();
        assert!(restored.cached_result.is_some());
        let cr = restored.cached_result.unwrap();
        assert_eq!(cr.output, "3 matches");
        assert!(!cr.is_error);
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
}
