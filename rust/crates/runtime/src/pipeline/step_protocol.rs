//! Step Protocol v1: The idempotent, versioned, cursor-aware execution unit.
//!
//! A Step is the smallest recoverable execution unit in the agent runtime.
//! It is NOT a Turn (UI concept). A Step can execute without any Turn,
//! and a single Turn may trigger multiple Steps.
//!
//! # Key properties
//!
//! - **Versioned**: `protocol_version` embedded in every Step and Checkpoint.
//!   Version mismatch → discard and restart (no migration in v1-v3).
//! - **Idempotent**: Each tool call gets an `IdempotencyKey`. On crash recovery,
//!   completed calls are skipped via cache lookup.
//! - **Cursor-aware**: `ExecutionCursor` tracks exact position within ACT phase
//!   (which tool call, what status). Resume from any point.
//! - **Retry-aware**: Per-step `RetryPolicy` with exponential backoff.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─── Protocol Version ────────────────────────────────────────────────────────

/// Current protocol version. Embedded in every Step and Checkpoint.
/// On deserialization: version mismatch → reject (no migration).
pub const PROTOCOL_VERSION: u32 = 1;

/// Check if a checkpoint's protocol version is compatible.
/// Returns Err with advice if not.
pub fn check_protocol_version(version: u32) -> Result<(), ProtocolError> {
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::VersionMismatch {
            expected: PROTOCOL_VERSION,
            found: version,
        });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProtocolError {
    VersionMismatch { expected: u32, found: u32 },
    InvalidCursor(String),
    CheckpointCorrupt(String),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VersionMismatch { expected, found } => {
                write!(
                    f,
                    "Protocol version mismatch: expected v{expected}, found v{found}. \
                     Discard checkpoint and restart."
                )
            }
            Self::InvalidCursor(msg) => write!(f, "Invalid execution cursor: {msg}"),
            Self::CheckpointCorrupt(msg) => write!(f, "Corrupt checkpoint: {msg}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

// ─── Step ────────────────────────────────────────────────────────────────────

/// The core execution unit. Not a Turn, not a Session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub step_id: String,
    pub task_id: String,
    pub dag_node_id: String,
    pub parent_step_id: Option<String>,
    pub action: StepAction,
    pub agent_id: Option<String>,
    pub payload: StepPayload,
    pub cursor: ExecutionCursor,
    pub checkpoint: Option<StepCheckpoint>,
    pub result: Option<StepResult>,
    pub status: StepStatus,
    pub idempotency_key: String,
    pub attempt: u32,
    pub max_attempts: u32,
    pub retry_policy: RetryPolicy,
    pub timeout_ms: u64,
    pub protocol_version: u32,
    pub created_at: u64,
    pub started_at: Option<u64>,
    pub completed_at: Option<u64>,
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
            step_id,
            task_id,
            dag_node_id,
            parent_step_id: None,
            action,
            agent_id: None,
            payload,
            cursor: ExecutionCursor::default(),
            checkpoint: None,
            result: None,
            status: StepStatus::Pending,
            idempotency_key,
            attempt: 1,
            max_attempts: 3,
            retry_policy: RetryPolicy::default(),
            timeout_ms: 300_000, // 5 minutes
            protocol_version: PROTOCOL_VERSION,
            created_at: epoch_ms(),
            started_at: None,
            completed_at: None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            StepStatus::Completed | StepStatus::Failed | StepStatus::Cancelled
        )
    }

    pub fn is_retriable(&self) -> bool {
        self.attempt < self.max_attempts
            && !matches!(
                self.status,
                StepStatus::Completed | StepStatus::Cancelled
            )
    }

    pub fn mark_started(&mut self, agent_id: &str) {
        self.agent_id = Some(agent_id.to_string());
        self.status = StepStatus::Running;
        self.started_at = Some(epoch_ms());
    }

    pub fn mark_completed(&mut self, result: StepResult) {
        self.result = Some(result);
        self.status = StepStatus::Completed;
        self.completed_at = Some(epoch_ms());
    }

    pub fn mark_failed(&mut self, error: &str) {
        self.result = Some(StepResult::Error {
            message: error.to_string(),
        });
        self.status = StepStatus::Failed;
        self.completed_at = Some(epoch_ms());
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
/// On crash recovery, resume from exactly this point.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionCursor {
    /// Current phase being executed
    pub phase: StepAction,
    /// Within ACT: which tool call (0-based). Other phases: 0.
    pub tool_index: u32,
    /// Per-tool completion tracking (ACT phase only)
    pub tool_completions: Vec<ToolCompletion>,
    /// Sub-step identifier (future: nested/composite steps)
    pub sub_step: Option<String>,
}

impl Default for ExecutionCursor {
    fn default() -> Self {
        Self {
            phase: StepAction::Perceive,
            tool_index: 0,
            tool_completions: Vec::new(),
            sub_step: None,
        }
    }
}

impl ExecutionCursor {
    /// Create cursor for an ACT step with N tool calls
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
                })
                .collect(),
            sub_step: None,
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCompletion {
    pub tool_name: String,
    pub call_id: String,
    pub status: ToolCompletionStatus,
    /// Points to idempotency cache entry (for crash recovery)
    pub idempotency_key: Option<String>,
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IdempotencyKey {
    pub step_id: String,
    pub tool_index: u32,
    pub content_hash: String,
}

impl IdempotencyKey {
    pub fn new(step_id: &str, tool_index: u32, tool_name: &str, args: &serde_json::Value) -> Self {
        let content_hash = compute_content_hash(tool_name, args);
        Self {
            step_id: step_id.to_string(),
            tool_index,
            content_hash,
        }
    }

    pub fn cache_key(&self) -> String {
        format!("{}:{}:{}", self.step_id, self.tool_index, self.content_hash)
    }
}

/// Cached tool result (for crash recovery).
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    // Canonical JSON (sorted keys)
    let canonical = serde_json::to_string(args).unwrap_or_default();
    hasher.update(canonical.as_bytes());
    let hash = hasher.finalize();
    format!("{:x}", hash)[..16].to_string() // 16-char prefix
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
        assert_eq!(step.status, StepStatus::Pending);
        assert_eq!(step.protocol_version, PROTOCOL_VERSION);
        assert_eq!(step.attempt, 1);
        assert_eq!(step.max_attempts, 3);
        assert!(!step.idempotency_key.is_empty());
        assert!(!step.is_terminal());
        assert!(step.is_retriable());
        assert!(step.agent_id.is_none());
        assert!(step.result.is_none());
        assert!(step.checkpoint.is_none());
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
        assert_eq!(step.status, StepStatus::Running);
        assert_eq!(step.agent_id.as_deref(), Some("agent-01"));
        assert!(step.started_at.is_some());

        step.mark_completed(StepResult::Act {
            tool_results_count: 1,
            assistant_text: Some("found it".into()),
            tokens_in: 100,
            tokens_out: 50,
        });
        assert!(step.is_terminal());
        assert!(!step.is_retriable());
        assert!(step.completed_at.is_some());
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
        // attempt=1, max=3, so would be retriable... but status is Failed (terminal)
        // is_retriable checks attempt < max AND not cancelled
        assert!(step.is_retriable()); // can retry (scheduler decides)
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
        };
        cursor.tool_completions[1] = ToolCompletion {
            tool_name: "bash".into(),
            call_id: "c2".into(),
            status: ToolCompletionStatus::Running,
            idempotency_key: None,
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
        assert_eq!(restored.step_id, "step-001");
        assert_eq!(restored.protocol_version, PROTOCOL_VERSION);
        assert_eq!(restored.action, StepAction::Act);
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
}
