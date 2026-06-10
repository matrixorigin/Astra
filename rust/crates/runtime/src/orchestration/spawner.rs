//! Dynamic agent spawner — runtime agent creation and lifecycle management.

use crate::messaging::SubRunInfo;
use astra_messaging::router::AgentMailboxRouter;
use astra_messaging::types::AgentAddress;
use astra_turn_core::fork_prefix_store::PrefixCaptureSink;
use astra_turn_core::fork_reconstruct::reconstruct_messages;
use astra_turn_core::fork_resolve::{
    PrefixResolveOutcome, SpawnResolveContext, resolve_inherit_prefix,
};
use astra_turn_core::orchestration_context_cache::SharedContextCache;
use astra_turn_core::orchestration_fanout_group::{
    AgentFanoutGroupProjection, AgentFanoutSlotIdentity, AgentFanoutSlotStatus,
};
use astra_turn_core::orchestration_progress::{
    AgentProgressEvent, ProgressBroadcaster, ProgressEventType,
};
use astra_turn_core::orchestration_spawn_tool::{SpawnAgentInput, SpawnAgentOutput};
use astra_turn_core::trace_event::{TraceContext, TraceEvent, TraceEventWriter};

use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::RwLock;
use uuid::Uuid;

use async_trait::async_trait;

use super::agent_trace_terminal_event_type;

// ─── Constants ──────────────────────────────────────────────────────────────

/// Sentinel run ID for the top-level (root) agent.
pub const ROOT_RUN_ID: &str = "root";
/// Maximum number of fanout groups tracked concurrently. Beyond this
/// limit, new groups are rejected with `SpawnError::FanoutGroupLimitExceeded`
/// to prevent unbounded memory growth in long-running sessions.
pub const MAX_FANOUT_GROUPS: usize = 64;
pub const SPAWN_STATUS_COMPLETED: &str = "completed";
pub const SPAWN_STATUS_INTERRUPTED: &str = "interrupted";
pub const SPAWN_STATUS_CANCELLED: &str = "cancelled";
pub const SPAWN_STATUS_FAILED: &str = "failed";
pub const SPAWN_STATUS_WAITING: &str = "waiting";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnRunStatusKind {
    Completed,
    Interrupted,
    Cancelled,
    Failed,
    Waiting,
    Other,
}

pub fn spawn_run_status_kind(status: &str) -> SpawnRunStatusKind {
    match status {
        SPAWN_STATUS_COMPLETED => SpawnRunStatusKind::Completed,
        SPAWN_STATUS_INTERRUPTED => SpawnRunStatusKind::Interrupted,
        SPAWN_STATUS_CANCELLED => SpawnRunStatusKind::Cancelled,
        SPAWN_STATUS_FAILED => SpawnRunStatusKind::Failed,
        SPAWN_STATUS_WAITING => SpawnRunStatusKind::Waiting,
        _ => SpawnRunStatusKind::Other,
    }
}

pub fn spawn_completion_status_from_finish_reason(finish_reason: Option<&str>) -> &'static str {
    match finish_reason {
        None | Some("normal") => SPAWN_STATUS_COMPLETED,
        Some(_) => SPAWN_STATUS_INTERRUPTED,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpawnStatusProjection {
    pub status: &'static str,
    pub finish_reason: &'static str,
    pub error: Option<String>,
}

pub(crate) fn project_subrun_status_to_spawn(
    subrun_status: &str,
    error: Option<String>,
) -> SpawnStatusProjection {
    let (status, finish_reason) = match subrun_status {
        astra_core::STATUS_COMPLETED => (SPAWN_STATUS_COMPLETED, "normal"),
        astra_core::STATUS_WAITING => (SPAWN_STATUS_WAITING, "waiting"),
        astra_core::STATUS_CANCELLED => (SPAWN_STATUS_CANCELLED, "cancelled"),
        astra_core::STATUS_FAILED => (SPAWN_STATUS_FAILED, "failed"),
        astra_core::STATUS_PAUSED => (SPAWN_STATUS_INTERRUPTED, SPAWN_STATUS_INTERRUPTED),
        _ => (SPAWN_STATUS_FAILED, "unknown"),
    };

    let error = if status == SPAWN_STATUS_FAILED {
        error.or_else(|| Some(format!("server spawned agent ended with {subrun_status}")))
    } else {
        error
    };

    SpawnStatusProjection {
        status,
        finish_reason,
        error,
    }
}

fn spawn_run_failure_message(run_result: &SpawnRunResult) -> String {
    match spawn_run_status_kind(&run_result.status) {
        SpawnRunStatusKind::Failed => run_result
            .error
            .clone()
            .unwrap_or_else(|| "agent run failed".to_string()),
        SpawnRunStatusKind::Other => format!(
            "agent run ended with unknown status '{}'",
            run_result.status
        ),
        _ => run_result.error.clone().unwrap_or_else(|| {
            format!(
                "agent run ended unexpectedly with status '{}'",
                run_result.status
            )
        }),
    }
}

fn spawn_run_result_to_agent_status(run_result: &SpawnRunResult) -> AgentStatus {
    match spawn_run_status_kind(&run_result.status) {
        // Cancellations from the spawn-run path are NOT user-driven —
        // they come from sub-process exit codes, depth violations, etc.
        // The user-driven path uses `cancel_agent` directly with
        // `AgentStatus::cancelled_by_user`.
        SpawnRunStatusKind::Cancelled => AgentStatus::cancelled_anonymous(),
        SpawnRunStatusKind::Failed | SpawnRunStatusKind::Other => AgentStatus::Failed {
            error: spawn_run_failure_message(run_result),
            finish_reason: Some(run_result.finish_reason.clone()),
        },
        SpawnRunStatusKind::Waiting => AgentStatus::Idle,
        SpawnRunStatusKind::Completed | SpawnRunStatusKind::Interrupted => AgentStatus::Completed {
            result: run_result.output.clone().unwrap_or_default(),
            finish_reason: Some(run_result.finish_reason.clone()),
        },
    }
}

fn fanout_slot_status_from_agent_status(
    status: &AgentStatus,
) -> (AgentFanoutSlotStatus, Option<String>) {
    match status {
        AgentStatus::Completed {
            result: _,
            finish_reason,
        } => {
            let reason = finish_reason
                .as_deref()
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
                .unwrap_or("normal");
            if reason == "normal" {
                (AgentFanoutSlotStatus::Completed, None)
            } else if is_parent_budget_finish_reason(reason) {
                (
                    AgentFanoutSlotStatus::CancelledByParentBudget,
                    Some(reason.to_string()),
                )
            } else {
                (AgentFanoutSlotStatus::Failed, Some(reason.to_string()))
            }
        }
        AgentStatus::Failed {
            error,
            finish_reason,
        } => (
            AgentFanoutSlotStatus::Failed,
            finish_reason.clone().or_else(|| Some(error.clone())),
        ),
        AgentStatus::Cancelled { by_user, reason } => {
            let reason = if reason.is_empty() {
                None
            } else {
                Some(reason.clone())
            };
            if *by_user {
                (AgentFanoutSlotStatus::CancelledByUser, reason)
            } else {
                (AgentFanoutSlotStatus::CancelledByParentBudget, reason)
            }
        }
        AgentStatus::Initializing | AgentStatus::Running { .. } | AgentStatus::Idle => {
            (AgentFanoutSlotStatus::TimedOut, None)
        }
    }
}

fn fanout_group_title(identity: &AgentFanoutSlotIdentity, title: Option<&str>) -> String {
    title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{} fanout", identity.group_id))
}

fn is_parent_budget_finish_reason(reason: &str) -> bool {
    matches!(
        reason,
        "budget_exhausted"
            | "turn_budget_exhausted"
            | "token_budget_exceeded"
            | "context_overflow"
            | "max_turns_exceeded"
            | "max_turns"
    )
}

fn spawn_run_result_to_sync_output(
    agent_id: String,
    run_result: SpawnRunResult,
    duration_ms: u64,
) -> SpawnAgentOutput {
    match spawn_run_status_kind(&run_result.status) {
        SpawnRunStatusKind::Cancelled => SpawnAgentOutput::Cancelled {
            agent_id,
            reason: run_result
                .output
                .unwrap_or_else(|| SPAWN_STATUS_CANCELLED.to_string()),
            tool_calls: run_result.tool_calls,
            duration_ms,
        },
        SpawnRunStatusKind::Waiting => SpawnAgentOutput::Waiting {
            agent_id,
            reason: run_result.output.unwrap_or_default(),
            tool_calls: run_result.tool_calls,
            duration_ms,
        },
        SpawnRunStatusKind::Failed | SpawnRunStatusKind::Other => SpawnAgentOutput::Failed {
            error: spawn_run_failure_message(&run_result),
            duration_ms,
        },
        SpawnRunStatusKind::Completed => SpawnAgentOutput::Completed {
            agent_id,
            result: run_result.output.unwrap_or_default(),
            tool_calls: run_result.tool_calls,
            duration_ms,
        },
        SpawnRunStatusKind::Interrupted => SpawnAgentOutput::Interrupted {
            agent_id,
            result: run_result.output.unwrap_or_default(),
            finish_reason: run_result.finish_reason,
            tool_calls: run_result.tool_calls,
            duration_ms,
        },
    }
}

fn agent_status_to_progress_event(
    status: &AgentStatus,
    metrics: &SpawnedAgentMetrics,
    started_at: SystemTime,
) -> Option<ProgressEventType> {
    match status {
        AgentStatus::Running { activity } => Some(ProgressEventType::Busy {
            activity: activity.clone(),
        }),
        AgentStatus::Idle => Some(ProgressEventType::Idle),
        AgentStatus::Completed {
            result,
            finish_reason,
        } => {
            let duration_ms = started_at
                .elapsed()
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let reason = finish_reason
                .as_deref()
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
                .unwrap_or("normal");
            if reason == "normal" {
                Some(ProgressEventType::Completed {
                    result_summary: result.clone(),
                    total_tool_calls: metrics.tool_calls,
                    total_tokens: (metrics.prompt_tokens, metrics.completion_tokens),
                    duration_ms,
                })
            } else {
                Some(ProgressEventType::Interrupted {
                    reason: reason.to_string(),
                    partial_summary: result.clone(),
                    total_tool_calls: metrics.tool_calls,
                    total_tokens: (metrics.prompt_tokens, metrics.completion_tokens),
                    duration_ms,
                })
            }
        }
        AgentStatus::Failed { error, .. } => Some(ProgressEventType::Failed {
            error: error.clone(),
        }),
        AgentStatus::Cancelled { by_user, reason } => Some(ProgressEventType::Cancelled {
            reason: if reason.is_empty() {
                if *by_user {
                    "cancelled by user".to_string()
                } else {
                    "cancelled".to_string()
                }
            } else {
                reason.clone()
            },
        }),
        AgentStatus::Initializing => None,
    }
}

// ─── Spawn Context ──────────────────────────────────────────────────────────

/// Context provided by the parent agent when spawning a child.
#[derive(Debug, Clone)]
pub struct SpawnContext {
    /// The parent's run ID.
    pub parent_run_id: String,
    /// The parent's agent ID (for tracking delegation chains).
    pub parent_agent_id: String,
    /// Current nested agent/sub-run depth of the parent.
    pub recursion_depth: u8,
    /// Whether the parent is itself a fork child. Fork children must
    /// not request another inherited prefix; their prompt already
    /// contains an inherited parent prefix, so recursively forking
    /// would drift from the byte-exact cache chain.
    pub parent_is_fork_child: bool,
    /// Working directory for the spawned agent.
    pub working_dir: PathBuf,
    /// Permissions inherited from the parent agent.
    pub inherited_permissions: Option<super::permission_sync::InheritedPermissions>,
    /// Skills inherited from the parent agent (subset of parent's active skills).
    pub inherited_skills: Vec<String>,
    /// Optional live-event sink for child token/tool/status mirroring.
    pub live_event_sink: Option<astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
    /// DB trace identity shared with the parent Web turn.
    pub trace_context: Option<TraceContext>,
    /// Tool call id of the parent `agent.spawn` invocation.
    pub spawn_tool_call_id: Option<String>,
}

// ─── Agent Status ───────────────────────────────────────────────────────────

// Re-export from turn-core (canonical definitions live there).
pub use astra_turn_core::orchestration_types::{
    AgentStatus, SpawnedAgentInfo, SpawnedAgentMetrics,
};

/// Outcome of waiting on a child agent ID.
#[derive(Debug, Clone, PartialEq)]
pub enum WaitForAgentOutcome {
    /// The agent exists and produced a terminal status.
    Status(AgentStatus),
    /// The agent exists, but did not reach a terminal status before timeout.
    TimedOut,
    /// No agent with this exact runtime ID is known to the spawner.
    Unknown,
}

/// Permission summary for display purposes.
#[derive(Debug, Clone, Default)]
pub struct PermissionSummary {
    /// Permission mode (auto, plan, accept_edits, prompt, deny).
    pub mode: String,
    /// Number of explicit allow rules.
    pub allow_rules: u32,
    /// Number of explicit deny rules.
    pub deny_rules: u32,
    /// Whether this agent has a parent for permission escalation.
    pub has_parent: bool,
    /// Recent permission denials (tool names).
    pub recent_denials: Vec<String>,
}

// ─── Spawned Agent State ────────────────────────────────────────────────────

/// Full state of a spawned agent.
#[derive(Debug, Clone)]
pub struct SpawnedAgentState {
    pub agent_id: String,
    pub run_id: String,
    pub parent_run_id: String,
    pub agent_type: String,
    pub description: String,
    pub status: AgentStatus,
    pub messaging_address: Option<AgentAddress>,
    pub worktree_path: Option<PathBuf>,
    pub started_at: SystemTime,
    pub metrics: SpawnedAgentMetrics,
    /// Permission summary for this agent.
    pub permission_summary: PermissionSummary,
    pub parent_agent_id: String,
    pub trace_context: Option<TraceContext>,
    pub spawn_tool_call_id: Option<String>,
    pub run_in_background: bool,
    pub fanout_slot: Option<AgentFanoutSlotIdentity>,
}

// SpawnedAgentInfo is re-exported from orchestration_types above.

impl From<&SpawnedAgentState> for SpawnedAgentInfo {
    fn from(state: &SpawnedAgentState) -> Self {
        Self {
            agent_id: state.agent_id.clone(),
            run_id: state.run_id.clone(),
            parent_run_id: state.parent_run_id.clone(),
            agent_type: state.agent_type.clone(),
            description: state.description.clone(),
            status: state.status.clone(),
            started_at: state.started_at,
            metrics: state.metrics.clone(),
            has_permission_issues: state.metrics.tools_blocked > 0,
            run_in_background: state.run_in_background,
            fanout_slot: state.fanout_slot.clone(),
        }
    }
}

// ─── Spawn Agent Executor Trait ─────────────────────────────────────────────

/// Configuration for a spawned agent run.
pub struct SpawnRunConfig {
    /// Unique run ID.
    pub run_id: String,
    /// Agent ID (name@run_id).
    pub agent_id: String,
    /// Current nested agent/sub-run depth of the spawned child loop.
    pub recursion_depth: u8,
    /// The agent type (explore, code-review, task, general-purpose).
    pub agent_type: String,
    /// Detailed task prompt for the agent.
    pub task: String,
    /// System prompt addendum from agent type definition.
    pub system_prompt_addendum: String,
    /// Explicit model override to use. `None` means "inherit the
    /// session/server default" instead of forcing a built-in alias.
    pub model: Option<String>,
    /// Max turns allowed.
    pub max_turns: u32,
    /// Allowed tools for this agent type.
    pub allowed_tools: Vec<String>,
    /// Whether the agent is read-only.
    pub read_only: bool,
    /// Working directory for the agent.
    pub working_dir: PathBuf,
    /// Optional mailbox for inter-agent messaging.
    pub mailbox: Option<astra_messaging::router::AgentMailbox>,
    /// Optional progress emitter for broadcasting turn completion events.
    pub progress_emitter: Option<astra_turn_core::orchestration_progress::AgentProgressEmitter>,
    /// Optional shared context cache for cross-agent knowledge sharing.
    pub context_cache: Option<Arc<SharedContextCache>>,
    /// Inherited permissions from parent agent.
    pub inherited_permissions: Option<super::permission_sync::InheritedPermissions>,
    /// Parent agent address for permission requests (if this is a child agent).
    pub parent_address: Option<astra_messaging::types::AgentAddress>,
    /// Permission context for runtime permission management.
    /// Created from inherited_permissions or as a fresh root context.
    pub permission_context:
        Option<std::sync::Arc<tokio::sync::RwLock<super::permission_sync::PermissionSyncContext>>>,
    /// Skills inherited from parent agent.
    pub inherited_skills: Vec<String>,
    /// Optional live-event sink for child token/tool/status mirroring.
    pub live_event_sink: Option<astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
    /// Captured parent prefix for prompt-cache inheritance. Present
    /// only when the child spawn requested inherit_prefix AND the
    /// resolver returned `Resolved`. Executors (CLI / server) that
    /// implement fork-prefix consumption prepend
    /// `inherited_prefix.prefix_messages` to the child's state.messages
    /// and emit a `ForkCacheEvent` after the child's first response.
    /// Executors that don't yet support it can ignore this field — the
    /// child will simply run without cache inheritance (equivalent to
    /// the PR 4 soft-fallback path).
    pub inherited_prefix: Option<InheritedChildPrefix>,
    /// True when this child inherited a parent prefix (fork mode).
    /// Fork children are NOT allowed to fork again — prevents
    /// degenerate case where a forked child forks with its own prefix
    /// (which differs from the original parent's, breaking the cache
    /// reuse chain). Same semantics as Claude Code's `isInForkChild()`.
    pub is_fork_child: bool,
}

/// Payload an executor needs to consume an inherited prefix.
///
/// Assembled by the spawner at spawn time from a resolved
/// [`ForkPrefix`]. Held as a plain struct (not an `Arc<ForkPrefix>`)
/// so the executor sees exactly the inputs it needs without also
/// needing to know about prefix storage internals.
#[derive(Debug, Clone)]
pub struct InheritedChildPrefix {
    /// Cross-reference to the captured prefix. Forwarded into the
    /// `ForkCacheEvent` the executor emits after the child's first
    /// response, so telemetry can join back to the capture.
    pub prefix_id: String,
    /// Parent run id (for the same join key).
    pub parent_run_id: String,
    /// Provider-scoped model id the prefix was captured against;
    /// required for ForkCacheEvent payload.
    pub provider: astra_turn_core::fork_prefix::ProviderKind,
    /// Captured thinking metadata from the parent prefix. Executors use this to
    /// keep replay normalization active even when the child model selector does
    /// not carry an explicit `(thinking...)` suffix.
    pub thinking: Option<astra_turn_core::fork_prefix::ThinkingConfigSlice>,
    /// Reconstructed message array to prepend to the child's
    /// `state.messages` — the output of
    /// [`astra_turn_core::fork_reconstruct::reconstruct_messages`]
    /// called on the prefix with no additional suffix (the executor
    /// appends its own child task message).
    pub prefix_messages: Vec<serde_json::Value>,
    /// Frozen tool schemas from the parent's capture — each entry
    /// deserialized from `ToolSchemaEntry.canonical_bytes`. When
    /// present, the executor should use these instead of the live
    /// registry to ensure tool-schema-hash stability with the parent
    /// (cache key alignment). `None` if the prefix has no tool
    /// schemas (e.g., older captures, or capture happened before
    /// tool_schema hashing was wired).
    pub frozen_tool_schemas: Option<Vec<serde_json::Value>>,
    /// Estimated cache-eligible tokens from the parent's perspective.
    /// Used as the `expected_cache_read_tokens` baseline when the
    /// executor evaluates the child's first response for a
    /// `ForkCacheEvent`. Zero is a valid sentinel for "no estimate
    /// available" — the evaluator handles it via the degenerate
    /// branch in `evaluate_fork_cache`.
    pub expected_cache_read_tokens: u64,
}

impl std::fmt::Debug for SpawnRunConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnRunConfig")
            .field("run_id", &self.run_id)
            .field("agent_id", &self.agent_id)
            .field("recursion_depth", &self.recursion_depth)
            .field("agent_type", &self.agent_type)
            .field("task", &self.task)
            .field("model", &self.model)
            .field("max_turns", &self.max_turns)
            .field("mailbox", &self.mailbox.is_some())
            .finish()
    }
}

/// Result from a spawned agent run.
#[derive(Debug, Clone)]
pub struct SpawnRunResult {
    /// Agent ID.
    pub agent_id: String,
    /// Run ID.
    pub run_id: String,
    /// Final status (`"completed"` / `"interrupted"` / `"cancelled"` /
    /// `"failed"` / `"waiting"`).
    pub status: String,
    /// **Structured reason the run ended.** Unlike `status` this
    /// names the exact interruption path (`budget_exhausted`,
    /// `token_budget_exceeded`, `context_overflow`, ...) rather than
    /// only the coarse terminal bucket. Parents that care about "did
    /// the child actually finish the task or did it run out of
    /// turns" should switch on this field instead of regex-matching
    /// `output`.
    ///
    /// Values mirror [`astra_turn_core::interruption::InterruptionKind::label`]
    /// when the loop ended on an interruption. `"normal"` when the
    /// loop completed cleanly with no interruption. `"cancelled"`,
    /// `"failed"`, `"waiting"` repeat the legacy status for
    /// convenience.
    pub finish_reason: String,
    /// Output text (if completed).
    pub output: Option<String>,
    /// Error message (if failed).
    pub error: Option<String>,
    /// Total prompt tokens.
    pub prompt_tokens: u64,
    /// Total completion tokens.
    pub completion_tokens: u64,
    /// Total tool calls.
    pub tool_calls: u32,
    /// Final permission summary for UI/status surfaces.
    pub permission_summary: Option<PermissionSummary>,
    /// Number of permission requests sent to parent.
    pub permission_requests: u32,
    /// Number of permission requests approved by parent.
    pub permission_requests_approved: u32,
    /// Number of tools blocked by permission.
    pub tools_blocked: u32,
}

/// Trait for executing spawned agent runs.
///
/// Similar to `SubRunExecutor` but specifically for dynamic agent spawning.
/// CLI layer implements this to run the agentic loop.
#[async_trait]
pub trait SpawnAgentExecutor: Send + Sync {
    /// Execute a spawned agent run.
    async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String>;
}

// ─── Dynamic Agent Spawner ──────────────────────────────────────────────────

/// Handles dynamic agent creation at runtime.
///
/// This is the core component that allows LLMs to spawn sub-agents without
/// pre-defined team configurations.
pub struct DynamicAgentSpawner {
    /// For inter-agent messaging.
    mailbox_router: Arc<AgentMailboxRouter>,
    /// For tracking spawned agents.
    active_agents: Arc<RwLock<HashMap<String, SpawnedAgentState>>>,
    /// Progress event broadcaster.
    progress_broadcaster: Arc<ProgressBroadcaster>,
    /// Shared context cache for cross-agent knowledge sharing.
    context_cache: Arc<SharedContextCache>,
    /// Optional executor for running agents (provided by CLI layer).
    executor: Option<Arc<dyn SpawnAgentExecutor>>,
    /// Optional session ID for persisting agent state to journal.
    session_id: Option<String>,
    /// Agent type registry (builtins + user-defined).
    agent_registry: astra_turn_core::orchestration_team_config::AgentRegistry,
    /// Completed agents archive for history queries.
    completed_agents: Arc<RwLock<Vec<SpawnedAgentState>>>,
    /// JoinSet tracking all in-flight background agent tasks for graceful shutdown drain.
    /// Shared across `clone_for_task` clones so every background handle lands here.
    background_tasks: Arc<std::sync::Mutex<tokio::task::JoinSet<()>>>,
    /// Per-agent abort handles for background children so the parent can cancel
    /// a single lagging sub-agent without killing siblings.
    background_abort_handles: Arc<RwLock<HashMap<String, tokio::task::AbortHandle>>>,
    /// Agent IDs spawned in background mode, for result collection after drain.
    background_agent_ids: Arc<std::sync::Mutex<Vec<String>>>,
    /// Completion notifiers: `agent(action='get_result')` awaits these instead of polling.
    completion_notifiers: Arc<RwLock<HashMap<String, Arc<tokio::sync::Notify>>>>,
    /// Foreground sync agents that the user promoted with Ctrl+B while
    /// the parent tool call was waiting for the child result.
    foreground_promotion_requests: Arc<RwLock<HashSet<String>>>,
    /// Optional fork-prefix store for cache inheritance across
    /// parent/child spawns. When `None` (default), spawn behavior is
    /// identical to pre-fork-prefix builds — existing callers are
    /// unaffected until they opt in via `with_prefix_store`.
    prefix_store: Option<Arc<dyn PrefixCaptureSink>>,
    /// Resolve outcomes keyed by spawned agent_id. Populated on every
    /// spawn, including spawns that produced no inherit request
    /// (they record `Disabled`) so telemetry / observability layers
    /// can distinguish "nobody asked" from "asked but fell back".
    /// Size-bounded implicitly by agent lifecycle: the CLI layer
    /// should evict entries via `clear_prefix_resolve` when the
    /// corresponding agent completes.
    prefix_resolve_outcomes: Arc<RwLock<HashMap<String, PrefixResolveOutcome>>>,
    /// Optional DB-first trace writer for Web/server lifecycle events.
    trace_writer: Option<Arc<dyn TraceEventWriter>>,
    /// Optional cap on the number of agents that may be active
    /// concurrently. `None` (the default) preserves the historical
    /// unlimited behavior. When `Some(n)`, the (n+1)-th spawn while n
    /// are still active rejects with
    /// [`SpawnError::ConcurrencyLimitExceeded`] before any side
    /// effects (no mailbox, no executor, no journal write).
    ///
    /// Why a soft reject and not a queue: queueing would let a slow
    /// child block the user's prompt indefinitely; the LLM is the
    /// queue (it sees the rejection in the tool result and can retry
    /// or re-plan).
    max_concurrent_agents: Option<usize>,
    /// Fanout group accounting keyed by group id. Group target_count
    /// is a user/model invariant, not a derived live-agent count.
    /// Capped at [`MAX_FANOUT_GROUPS`] to prevent unbounded memory
    /// growth from long-running sessions.
    fanout_groups: Arc<RwLock<HashMap<String, AgentFanoutGroupProjection>>>,
}

impl DynamicAgentSpawner {
    /// Create a new spawner with the given dependencies.
    pub fn new(mailbox_router: Arc<AgentMailboxRouter>) -> Self {
        Self {
            mailbox_router,
            active_agents: Arc::new(RwLock::new(HashMap::new())),
            progress_broadcaster: Arc::new(ProgressBroadcaster::default()),
            context_cache: Arc::new(SharedContextCache::default()),
            executor: None,
            session_id: None,
            agent_registry:
                astra_turn_core::orchestration_team_config::AgentRegistry::builtins_only(),
            completed_agents: Arc::new(RwLock::new(Vec::new())),
            background_tasks: Arc::new(std::sync::Mutex::new(tokio::task::JoinSet::new())),
            background_abort_handles: Arc::new(RwLock::new(HashMap::new())),
            background_agent_ids: Arc::new(std::sync::Mutex::new(Vec::new())),
            completion_notifiers: Arc::new(RwLock::new(HashMap::new())),
            foreground_promotion_requests: Arc::new(RwLock::new(HashSet::new())),
            prefix_store: None,
            prefix_resolve_outcomes: Arc::new(RwLock::new(HashMap::new())),
            trace_writer: None,
            max_concurrent_agents: None,
            fanout_groups: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a new spawner with a shared progress broadcaster.
    ///
    /// Use this when delegation sub-runs also need to emit to the same broadcaster.
    pub fn with_broadcaster(
        mailbox_router: Arc<AgentMailboxRouter>,
        progress_broadcaster: Arc<ProgressBroadcaster>,
    ) -> Self {
        let mut s = Self::new(mailbox_router);
        s.progress_broadcaster = progress_broadcaster;
        s
    }

    /// Create a new spawner with a custom context cache.
    pub fn with_context_cache(
        mailbox_router: Arc<AgentMailboxRouter>,
        context_cache: Arc<SharedContextCache>,
    ) -> Self {
        let mut s = Self::new(mailbox_router);
        s.context_cache = context_cache;
        s
    }

    /// Set the executor for running spawned agents.
    pub fn with_executor(mut self, executor: Arc<dyn SpawnAgentExecutor>) -> Self {
        self.executor = Some(executor);
        self
    }

    /// Install a fork-prefix store. When set, `spawn` resolves any
    /// `InheritPrefixSpec` in the request and records the outcome
    /// for later query via `last_prefix_resolve`. When unset,
    /// `spawn` behaves as if inherit_prefix were never requested —
    /// fully backwards compatible with pre-fork-prefix callers.
    pub fn with_prefix_store(mut self, store: Arc<dyn PrefixCaptureSink>) -> Self {
        self.prefix_store = Some(store);
        self
    }

    pub fn with_trace_writer(mut self, writer: Arc<dyn TraceEventWriter>) -> Self {
        self.trace_writer = Some(writer);
        self
    }

    /// Cap the number of agents that may be active concurrently.
    ///
    /// `cap == 0` is rejected at the API boundary (no spawns would ever
    /// succeed); pass `None` via the default state to disable the cap
    /// entirely. The cap is on currently-active agents only — completed
    /// archives don't count, so a long-lived session never accumulates a
    /// permanent block.
    pub fn with_max_concurrent_agents(mut self, cap: usize) -> Self {
        debug_assert!(
            cap > 0,
            "max_concurrent_agents=0 disables all spawns; pass None to disable the cap entirely"
        );
        self.max_concurrent_agents = Some(cap.max(1));
        self
    }

    /// Read-only access to the installed prefix store. Exposed so
    /// callers (e.g. the CLI loop host that captures parent turns)
    /// can share the same Arc the spawner already holds, guaranteeing
    /// captured prefixes are visible at spawn time. Returns `None`
    /// when no store is wired.
    pub fn prefix_store(&self) -> Option<&Arc<dyn PrefixCaptureSink>> {
        self.prefix_store.as_ref()
    }

    /// Query the resolve outcome recorded for a spawned agent.
    /// Returns `None` if the agent was never spawned by this
    /// spawner, or if its outcome has been cleared via
    /// `clear_prefix_resolve`. Every successful `spawn` records
    /// exactly one outcome, even in the no-inherit case (recorded
    /// as `Disabled`).
    pub async fn last_prefix_resolve(&self, agent_id: &str) -> Option<PrefixResolveOutcome> {
        self.prefix_resolve_outcomes
            .read()
            .await
            .get(agent_id)
            .cloned()
    }

    /// Drop the recorded resolve outcome for an agent. CLI layers
    /// should call this when the agent completes so the map
    /// doesn't grow unbounded over a long-running runtime process.
    pub async fn clear_prefix_resolve(&self, agent_id: &str) {
        self.prefix_resolve_outcomes.write().await.remove(agent_id);
    }

    pub async fn list_fanout_groups(&self) -> Vec<AgentFanoutGroupProjection> {
        let mut groups: Vec<_> = self.fanout_groups.read().await.values().cloned().collect();
        groups.sort_by(|a, b| a.group_id.cmp(&b.group_id));
        groups
    }

    pub async fn fanout_group_for_agent(
        &self,
        agent_id: &str,
    ) -> Option<AgentFanoutGroupProjection> {
        self.fanout_groups
            .read()
            .await
            .values()
            .find(|group| {
                group
                    .slots
                    .iter()
                    .any(|slot| slot.agent_id.as_deref() == Some(agent_id))
            })
            .cloned()
    }

    fn remember_background_agent_id(&self, agent_id: &str) {
        let mut ids = self
            .background_agent_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !ids.iter().any(|id| id == agent_id) {
            ids.push(agent_id.to_string());
        }
    }

    fn reap_finished_agent_tasks(&self) {
        let Ok(mut tasks) = self.background_tasks.lock() else {
            return;
        };
        while let Some(result) = tasks.try_join_next() {
            if let Err(error) = result
                && error.is_panic()
            {
                astra_core::agent_warn!("spawner", "agent task panicked during finished-task reap");
            }
        }
    }

    async fn take_foreground_promotion_request(&self, agent_id: &str) -> bool {
        self.foreground_promotion_requests
            .write()
            .await
            .remove(agent_id)
    }

    /// Promote the newest foreground sync agent for `parent_run_id` into
    /// background mode. The agent keeps running; the waiting
    /// `agent(action='spawn')` call wakes and returns `Launched`, so the
    /// parent can continue and collect with `agent(action='get_result')`.
    pub async fn promote_foreground_agent_to_background(
        &self,
        parent_run_id: Option<&str>,
    ) -> Option<SpawnedAgentInfo> {
        let promoted = {
            let mut active_agents = self.active_agents.write().await;
            let agent_id = active_agents
                .iter()
                .filter(|(_, state)| {
                    !state.run_in_background
                        && parent_run_id.is_none_or(|run_id| state.parent_run_id == run_id)
                })
                .max_by_key(|(_, state)| {
                    state
                        .started_at
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|duration| duration.as_millis())
                        .unwrap_or(0)
                })
                .map(|(agent_id, _)| agent_id.clone())?;
            let state = active_agents.get_mut(&agent_id)?;
            state.run_in_background = true;
            SpawnedAgentInfo::from(&*state)
        };

        self.remember_background_agent_id(&promoted.agent_id);
        self.foreground_promotion_requests
            .write()
            .await
            .insert(promoted.agent_id.clone());
        if let Some(notifier) = self
            .completion_notifiers
            .read()
            .await
            .get(&promoted.agent_id)
            .cloned()
        {
            notifier.notify_waiters();
        }
        Some(promoted)
    }

    async fn record_fanout_spawn_accepted(
        &self,
        identity: &AgentFanoutSlotIdentity,
        group_title: Option<&str>,
        agent_id: &str,
        agent_type: &str,
        description: &str,
        created_by_tool_use_id: Option<&str>,
    ) -> Result<(), SpawnError> {
        let mut groups = self.fanout_groups.write().await;
        let is_new = !groups.contains_key(&identity.group_id);
        if is_new {
            self.evict_terminal_fanout_group_if_full(&mut groups);
        }
        let group = groups.entry(identity.group_id.clone()).or_insert_with(|| {
            let mut group = AgentFanoutGroupProjection::new(
                identity.group_id.clone(),
                fanout_group_title(identity, group_title),
                identity.target_count,
            );
            group.created_by_tool_use_id = created_by_tool_use_id.map(ToString::to_string);
            group
        });
        if group.target_count != identity.target_count {
            return Err(SpawnError::InvalidInput(format!(
                "fanout group '{}' target_count changed from {} to {}",
                identity.group_id, group.target_count, identity.target_count
            )));
        }
        group
            .set_slot_request(identity.slot_index, agent_type, description)
            .map_err(SpawnError::InvalidInput)?;
        group
            .record_spawn_accepted(identity.slot_index, agent_id)
            .map_err(SpawnError::InvalidInput)?;
        group.touch();
        Ok(())
    }

    async fn record_fanout_spawn_rejected(
        &self,
        identity: &AgentFanoutSlotIdentity,
        group_title: Option<&str>,
        agent_type: &str,
        description: &str,
        reason: impl Into<String>,
        created_by_tool_use_id: Option<&str>,
    ) -> Result<(), SpawnError> {
        let mut groups = self.fanout_groups.write().await;
        let is_new = !groups.contains_key(&identity.group_id);
        if is_new {
            self.evict_terminal_fanout_group_if_full(&mut groups);
        }
        let group = groups.entry(identity.group_id.clone()).or_insert_with(|| {
            let mut group = AgentFanoutGroupProjection::new(
                identity.group_id.clone(),
                fanout_group_title(identity, group_title),
                identity.target_count,
            );
            group.created_by_tool_use_id = created_by_tool_use_id.map(ToString::to_string);
            group
        });
        if group.target_count != identity.target_count {
            return Err(SpawnError::InvalidInput(format!(
                "fanout group '{}' target_count changed from {} to {}",
                identity.group_id, group.target_count, identity.target_count
            )));
        }
        group
            .set_slot_request(identity.slot_index, agent_type, description)
            .map_err(SpawnError::InvalidInput)?;
        group
            .record_spawn_rejected(identity.slot_index, reason)
            .map_err(SpawnError::InvalidInput)?;
        group.touch();
        Ok(())
    }

    async fn record_fanout_spawn_rejected_for_input(
        &self,
        fanout_slot: Option<&AgentFanoutSlotIdentity>,
        input: &SpawnAgentInput,
        context: &SpawnContext,
        reason: impl Into<String>,
    ) {
        if let Some(identity) = fanout_slot {
            let _ = self
                .record_fanout_spawn_rejected(
                    identity,
                    input.fanout_group_title.as_deref(),
                    &input.agent_type,
                    &input.description,
                    reason,
                    context.spawn_tool_call_id.as_deref(),
                )
                .await;
        }
    }

    async fn record_fanout_terminal_state(&self, state: &SpawnedAgentState) {
        let Some(identity) = state.fanout_slot.as_ref() else {
            return;
        };
        let (status, reason) = fanout_slot_status_from_agent_status(&state.status);
        let mut groups = self.fanout_groups.write().await;
        let Some(group) = groups.get_mut(&identity.group_id) else {
            return;
        };
        let _ = group.record_terminal_by_agent(&state.agent_id, status, reason);
        group.touch();
    }

    /// Evict the least-recently-touched terminal group when the fanout-groups
    /// map is at capacity.  Only terminal groups (Finished / Incomplete) are
    /// candidates — evicting a live group would corrupt in-flight agent
    /// accounting.  If no terminal candidate exists the map is allowed to
    /// grow one extra slot so the caller's insert can proceed.
    fn evict_terminal_fanout_group_if_full(
        &self,
        groups: &mut HashMap<String, AgentFanoutGroupProjection>,
    ) {
        if groups.len() < MAX_FANOUT_GROUPS {
            return;
        }
        // Find the terminal group with the oldest last_touched.
        let Some((evict_id, _)) = groups
            .iter()
            .filter(|(_, g)| g.is_terminal())
            .min_by_key(|(_, g)| g.last_touched)
        else {
            return;
        };
        let evict_id = evict_id.clone();
        groups.remove(&evict_id);
    }

    async fn mark_fanout_result_collected(&self, state: &SpawnedAgentState) {
        let Some(identity) = state.fanout_slot.as_ref() else {
            return;
        };
        let mut groups = self.fanout_groups.write().await;
        if let Some(group) = groups.get_mut(&identity.group_id) {
            group.mark_result_collected(&state.agent_id);
            group.touch();
        }
    }

    /// Enable journal persistence for agent lifecycle events.
    pub fn with_session(mut self, session_id: String) -> Self {
        self.session_id = Some(session_id);
        self
    }

    fn trace_event_id(kind: &str, parts: &[&str]) -> String {
        let mut hasher = Sha256::new();
        for part in parts {
            hasher.update(part.as_bytes());
            hasher.update([0]);
        }
        let digest = hasher.finalize();
        let hash = digest[..12]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        format!("trace:{kind}:{hash}")
    }

    async fn write_trace_event(&self, event: TraceEvent) {
        let Some(writer) = self.trace_writer.as_ref() else {
            return;
        };
        if let Err(error) = writer.write(event).await {
            astra_core::agent_error!(
                "spawner",
                "failed to persist agent lifecycle trace: {error}"
            );
        }
    }

    async fn emit_agent_spawned_trace(&self, state: &SpawnedAgentState) {
        let Some(trace) = state.trace_context.as_ref() else {
            return;
        };
        let mut event = TraceEvent::new(
            Self::trace_event_id("agent_spawned", &[&state.run_id]),
            trace.session_id.clone(),
            trace.user_id.clone(),
            "agent_spawned",
            "agent_lifecycle",
        )
        .with_turn_context(trace);
        event.run_id = Some(state.run_id.clone());
        event.parent_run_id = Some(state.parent_run_id.clone());
        event.agent_id = Some(state.agent_id.clone());
        event.parent_agent_id = Some(state.parent_agent_id.clone());
        event.tool_call_id = state.spawn_tool_call_id.clone();
        event.parent_event_id = Some(trace.root_event_id.clone());
        event.metadata = serde_json::json!({
            "agent_type": &state.agent_type,
            "description": &state.description,
            "status": "spawned",
            "spawn_tool_call_id": &state.spawn_tool_call_id,
            "run_in_background": state.run_in_background,
            "fanout_slot": state.fanout_slot.as_ref().map(|slot| serde_json::json!({
                "group_id": &slot.group_id,
                "target_count": slot.target_count,
                "slot_index": slot.slot_index,
            })),
        });
        self.write_trace_event(event).await;
    }

    async fn emit_agent_terminal_trace(
        &self,
        state: &SpawnedAgentState,
        status: &str,
        finish_reason: Option<&str>,
        output: Option<&str>,
        error: Option<&str>,
    ) {
        let Some(trace) = state.trace_context.as_ref() else {
            return;
        };
        let event_type = agent_trace_terminal_event_type(status);
        let duration_ms = state
            .started_at
            .elapsed()
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut event = TraceEvent::new(
            Self::trace_event_id("agent_terminal", &[&state.run_id, event_type]),
            trace.session_id.clone(),
            trace.user_id.clone(),
            event_type,
            "agent_lifecycle",
        )
        .with_turn_context(trace);
        event.run_id = Some(state.run_id.clone());
        event.parent_run_id = Some(state.parent_run_id.clone());
        event.agent_id = Some(state.agent_id.clone());
        event.parent_agent_id = Some(state.parent_agent_id.clone());
        event.tool_call_id = state.spawn_tool_call_id.clone();
        event.meta_duration_ms = i32::try_from(duration_ms).ok();
        event.parent_event_id = Some(trace.root_event_id.clone());
        event.metadata = serde_json::json!({
            "status": status,
            "finish_reason": finish_reason,
            "prompt_tokens": state.metrics.prompt_tokens,
            "completion_tokens": state.metrics.completion_tokens,
            "tool_calls": state.metrics.tool_calls,
            "result_preview": output.map(|text| text.chars().take(500).collect::<String>()),
            "error": error.map(|text| text.chars().take(500).collect::<String>()),
        });
        self.write_trace_event(event).await;
    }
    /// Get a reference to the agent registry.
    pub fn agent_registry(&self) -> &astra_turn_core::orchestration_team_config::AgentRegistry {
        &self.agent_registry
    }

    /// Get the shared context cache.
    pub fn context_cache(&self) -> &Arc<SharedContextCache> {
        &self.context_cache
    }

    /// Check if an executor is configured.
    pub fn has_executor(&self) -> bool {
        self.executor.is_some()
    }

    /// Expose the shared mailbox router for top-level coordination tools.
    pub fn mailbox_router(&self) -> Arc<AgentMailboxRouter> {
        self.mailbox_router.clone()
    }

    /// Spawn a new agent from the given specification.
    ///
    /// This is called by the `agent(action='spawn')` handler.
    pub async fn spawn(
        &self,
        input: SpawnAgentInput,
        context: &SpawnContext,
    ) -> Result<SpawnAgentOutput, SpawnError> {
        if context.parent_is_fork_child && input.inherit_prefix.is_some() {
            return Err(SpawnError::NestedForkInheritanceRejected);
        }

        let fanout_slot = input
            .fanout_slot_identity()
            .map_err(SpawnError::InvalidInput)?;

        // 1. Validate agent type
        let agent_def = match self.agent_registry.get(&input.agent_type) {
            Some(agent_def) => agent_def,
            None => {
                self.record_fanout_spawn_rejected_for_input(
                    fanout_slot.as_ref(),
                    &input,
                    context,
                    format!("unknown agent type: {}", input.agent_type),
                )
                .await;
                return Err(SpawnError::UnknownAgentType(input.agent_type.clone()));
            }
        };
        let child_recursion_depth =
            match astra_turn_core::agentic_recursion_guard::checked_child_recursion_depth(
                context.recursion_depth,
            ) {
                Ok(depth) => depth,
                Err(error) => {
                    self.record_fanout_spawn_rejected_for_input(
                        fanout_slot.as_ref(),
                        &input,
                        context,
                        format!("recursion depth limit exceeded: {error}"),
                    )
                    .await;
                    return Err(SpawnError::DepthLimitExceeded(error));
                }
            };

        // 2. Generate IDs
        let agent_name = input
            .name
            .clone()
            .unwrap_or_else(|| format!("{}_{}", input.agent_type, &Uuid::new_v4().to_string()));
        let run_id = Uuid::new_v4().to_string();
        let agent_id = format!("{}@{}", agent_name, &run_id);

        // 3. Determine model and turns
        let model = input
            .model
            .clone()
            .or_else(|| agent_def.default_model.clone());
        // Budget resolution: explicit `max_turns` wins, else the
        // `complexity` hint scales the agent-type default, else the
        // default is used as-is. See `resolve_turn_budget`.
        let max_turns = astra_turn_core::orchestration_spawn_tool::resolve_turn_budget(
            input.max_turns,
            input.complexity.as_deref(),
            agent_def.max_turns,
        );
        // 3b. Resolve fork-prefix inheritance before any side effects
        // (mailbox, worktree, active_agents state). A hard-fail from
        // `required=true` must NOT leave half-constructed state
        // behind; soft-fallback recording only happens for spawns
        // that are definitely going to succeed.
        //
        // The resolver itself is a pure function over a store; if no
        // store is configured we skip even building the context
        // (saves a clone + RwLock write in the common path).
        let resolve_outcome =
            if let (Some(store), Some(model)) = (self.prefix_store.as_ref(), model.as_ref()) {
                // Infer the child's provider from the model string via
                // the same normalization scheme PR 1 uses for capture
                // (`ProviderKind::from_provider_hint`). Ensures the
                // child's resolve context matches the provider that
                // captured the parent prefix, so
                // Anthropic-captured prefixes resolve for Anthropic
                // children, OpenAI-captured for OpenAI, etc. Providers
                // that astra doesn't yet wire-compatibly reconstruct
                // for (OpenAI / Bedrock / Other) will still resolve
                // against a matching capture and carry the prefix into
                // SpawnRunConfig; executor-side consumption is gated
                // by the sink the caller installs.
                let child_provider =
                    astra_turn_core::fork_prefix::ProviderKind::from_provider_hint(model);
                let resolve_ctx = SpawnResolveContext {
                    caller_run_id: Some(context.parent_run_id.clone()),
                    child_provider,
                    child_model_id: model.clone(),
                    child_max_output_tokens: input.max_output_tokens,
                };
                resolve_inherit_prefix(input.inherit_prefix.as_ref(), &resolve_ctx, store.as_ref())
            } else {
                // No prefix store, or no concrete child model to validate
                // provider/model compatibility against. In both cases the
                // child proceeds without inherited prefix and relies on the
                // session/server default model.
                PrefixResolveOutcome::Disabled
            };
        if let PrefixResolveOutcome::Failed { reason } = &resolve_outcome {
            self.record_fanout_spawn_rejected_for_input(
                fanout_slot.as_ref(),
                &input,
                context,
                format!("required prefix inheritance failed: {reason:?}"),
            )
            .await;
            return Err(SpawnError::PrefixInheritanceRequired {
                reason: format!("{reason:?}"),
            });
        }

        // 4. Reserve active-agent capacity under the same write lock that
        // inserts the agent. This closes the read-check/write-insert TOCTOU
        // gap when several named/background spawns all await mailbox setup.
        let permission_summary = build_permission_summary(context);
        let state = SpawnedAgentState {
            agent_id: agent_id.clone(),
            run_id: run_id.clone(),
            parent_run_id: context.parent_run_id.clone(),
            agent_type: input.agent_type.clone(),
            description: input.description.clone(),
            status: AgentStatus::Initializing,
            messaging_address: None,
            worktree_path: None,
            started_at: SystemTime::now(),
            metrics: Default::default(),
            permission_summary,
            parent_agent_id: context.parent_agent_id.clone(),
            trace_context: context.trace_context.clone(),
            spawn_tool_call_id: context.spawn_tool_call_id.clone(),
            run_in_background: input.run_in_background,
            fanout_slot: fanout_slot.clone(),
        };
        {
            let mut active_agents = self.active_agents.write().await;
            if let Some(limit) = self.max_concurrent_agents {
                let active = active_agents.len();
                if active >= limit {
                    drop(active_agents);
                    if let Some(identity) = fanout_slot.as_ref() {
                        let _ = self
                            .record_fanout_spawn_rejected(
                                identity,
                                input.fanout_group_title.as_deref(),
                                &input.agent_type,
                                &input.description,
                                format!("concurrency limit reached: {active}/{limit} active"),
                                context.spawn_tool_call_id.as_deref(),
                            )
                            .await;
                    }
                    return Err(SpawnError::ConcurrencyLimitExceeded { active, limit });
                }
            }
            active_agents.insert(agent_id.clone(), state);
        }

        // 5. Register mailbox if named
        let mailbox = if input.name.is_some() {
            let addr = AgentAddress::new(&run_id, &agent_id);
            let delegation_id = Some(context.parent_run_id.clone());
            match self
                .mailbox_router
                .register(addr.clone(), delegation_id)
                .await
            {
                Ok(mb) => Some(mb),
                Err(e) => {
                    self.active_agents.write().await.remove(&agent_id);
                    self.record_fanout_spawn_rejected_for_input(
                        fanout_slot.as_ref(),
                        &input,
                        context,
                        format!("mailbox registration failed: {e}"),
                    )
                    .await;
                    return Err(SpawnError::MailboxRegistration(e.to_string()));
                }
            }
        } else {
            None
        };

        let messaging_address = mailbox.as_ref().map(|mb| mb.address.clone());
        if messaging_address.is_some() {
            let depth = self
                .mailbox_router
                .run_depth(&context.parent_run_id)
                .await
                .unwrap_or(0)
                + 1;
            self.mailbox_router
                .record_sub_run(SubRunInfo {
                    run_id: run_id.clone(),
                    parent_run_id: context.parent_run_id.clone(),
                    delegation_id: context.parent_run_id.clone(),
                    agent_id: agent_id.clone(),
                    depth,
                })
                .await;
        }

        // 5b. Create isolated worktree if requested
        let worktree_path = if input.isolated {
            match create_agent_worktree(&context.working_dir, &run_id) {
                Ok(path) => Some(path),
                Err(e) => {
                    self.active_agents.write().await.remove(&agent_id);
                    if let Some(addr) = messaging_address.as_ref() {
                        let _ = self.mailbox_router.unregister(addr).await;
                    }
                    self.record_fanout_spawn_rejected_for_input(
                        fanout_slot.as_ref(),
                        &input,
                        context,
                        format!("worktree creation failed: {e}"),
                    )
                    .await;
                    return Err(SpawnError::WorktreeCreation(format!(
                        "failed to create worktree for {agent_id}: {e}"
                    )));
                }
            }
        } else {
            None
        };

        let spawned_state_for_trace = {
            let mut active_agents = self.active_agents.write().await;
            let state = match active_agents.get_mut(&agent_id) {
                Some(s) => s,
                None => {
                    // Agent was cancelled between reservation and spawn
                    // completion. Clean up and return a race error
                    // instead of panicking.
                    if let Some(addr) = messaging_address.as_ref() {
                        let _ = self.mailbox_router.unregister(addr).await;
                    }
                    return Err(SpawnError::Race(format!(
                        "agent {agent_id} was cancelled before spawn completed"
                    )));
                }
            };
            state.messaging_address = messaging_address.clone();
            state.worktree_path = worktree_path.clone();
            state.clone()
        };
        if let Some(identity) = fanout_slot.as_ref()
            && let Err(error) = self
                .record_fanout_spawn_accepted(
                    identity,
                    input.fanout_group_title.as_deref(),
                    &agent_id,
                    &input.agent_type,
                    &input.description,
                    context.spawn_tool_call_id.as_deref(),
                )
                .await
        {
            self.active_agents.write().await.remove(&agent_id);
            if let Some(addr) = messaging_address.as_ref() {
                let _ = self.mailbox_router.unregister(addr).await;
            }
            return Err(error);
        }
        self.emit_agent_spawned_trace(&spawned_state_for_trace)
            .await;

        // 6b. Reconstruct the inherited prefix payload for the
        // executor BEFORE moving resolve_outcome into the outcomes
        // map. Called only if the resolver produced `Resolved`;
        // all other outcomes (Disabled / Fallback / Failed) leave
        // `inherited_prefix` as None so the executor runs fresh.
        // Reconstruct errors are rare (would imply corrupt capture
        // bytes) — we degrade to None rather than fail the spawn,
        // mirroring soft-fallback semantics.
        let inherited_prefix = build_inherited_child_prefix(&resolve_outcome);

        // 6c. Record the resolve outcome. We do this after the
        // active_agents insert so any observer who sees the agent
        // via `list_agents` can safely look up its resolve outcome
        // without a race. Key is agent_id (not run_id) because
        // callers see agent_id in `SpawnAgentOutput::Launched`.
        self.prefix_resolve_outcomes
            .write()
            .await
            .insert(agent_id.clone(), resolve_outcome);

        // 7. Emit started event
        let emitter = self.progress_broadcaster.for_agent(agent_id.clone());
        emitter.started(&input.description);
        emitter.agent_spawned_with_fanout(
            &run_id,
            &context.parent_run_id,
            &input.agent_type,
            &input.description,
            fanout_slot.clone(),
        );

        // 7. Build parent address for permission requests
        let parent_address = astra_messaging::types::AgentAddress::new(
            &context.parent_run_id,
            &context.parent_agent_id,
        );

        // 7b. Build permission context from inherited permissions
        let permission_context = context.inherited_permissions.as_ref().map(|inherited| {
            let ctx = super::permission_sync::PermissionSyncContext::new(inherited.clone());
            std::sync::Arc::new(tokio::sync::RwLock::new(ctx))
        });

        // 8. Build run config
        let run_config = SpawnRunConfig {
            run_id: run_id.clone(),
            agent_id: agent_id.clone(),
            recursion_depth: child_recursion_depth,
            agent_type: input.agent_type.clone(),
            task: input.prompt.clone(),
            system_prompt_addendum: agent_def.system_prompt_addendum.clone(),
            model,
            max_turns,
            allowed_tools: agent_def.allowed_tools.iter().cloned().collect(),
            read_only: agent_def.read_only,
            working_dir: worktree_path.unwrap_or_else(|| context.working_dir.clone()),
            mailbox,
            progress_emitter: Some(emitter.clone()),
            context_cache: Some(Arc::clone(&self.context_cache)),
            // Inherit permissions from parent context
            inherited_permissions: context.inherited_permissions.clone(),
            // Parent address for permission requests
            parent_address: Some(parent_address),
            // Permission context for runtime permission management
            permission_context,
            // Skills inherited from parent
            inherited_skills: context.inherited_skills.clone(),
            live_event_sink: context.live_event_sink.clone(),
            is_fork_child: inherited_prefix.is_some(),
            inherited_prefix,
        };

        // Emit agent_spawned journal event for unified timeline.
        if let Some(ref sid) = self.session_id {
            let evt = astra_services::session_journal::JournalEvent::agent_spawned(
                Some(sid),
                &agent_id,
                &run_id,
                &context.parent_run_id,
                &run_config.agent_type,
                &input.description,
                run_config.model.as_deref(),
                run_config.inherited_prefix.is_some(),
            );
            if let Ok(writer) = astra_services::session_journal::JournalWriter::new(sid) {
                let _ = writer.append(&evt);
            }
        }

        // 8. Execute or launch. Both explicit background spawns and
        // foreground sync spawns run through the same task/finalization
        // pipe. Sync mode simply waits for the terminal oneshot unless
        // Ctrl+B promotes the wait into a background `Launched` result.
        let Some(ref executor) = self.executor else {
            // No executor available - return as launched (degraded mode)
            return Ok(SpawnAgentOutput::Launched {
                agent_id,
                description: input.description,
                messaging_address: messaging_address.map(|a| a.to_string()),
            });
        };

        self.update_status(
            &agent_id,
            AgentStatus::Running {
                activity: "executing".to_string(),
            },
        )
        .await;

        let started_at = self
            .active_agents
            .read()
            .await
            .get(&agent_id)
            .map(|s| s.started_at)
            .unwrap_or_else(SystemTime::now);
        let description = input.description.clone();
        let messaging_address_text = messaging_address.as_ref().map(|a| a.to_string());
        let notify = Arc::new(tokio::sync::Notify::new());
        self.completion_notifiers
            .write()
            .await
            .insert(agent_id.clone(), Arc::clone(&notify));

        let (terminal_tx, mut terminal_rx) = tokio::sync::oneshot::channel();
        let executor = Arc::clone(executor);
        let spawner = self.clone_for_task();
        let agent_id_for_task = agent_id.clone();
        let agent_id_for_output = agent_id.clone();
        let notify_guard = Arc::clone(&notify);
        let notify_after_completion = Arc::clone(&notify);
        let spawn_future = async move {
            struct NotifyOnDrop(Arc<tokio::sync::Notify>);
            impl Drop for NotifyOnDrop {
                fn drop(&mut self) {
                    self.0.notify_waiters();
                }
            }
            let guard = NotifyOnDrop(notify_guard);
            let result = executor.execute(run_config).await;
            let output = match result {
                Ok(run_result) => {
                    let status = spawn_run_result_to_agent_status(&run_result);
                    spawner
                        .finalize_background_agent(
                            &agent_id_for_task,
                            status,
                            &run_result.status,
                            Some(run_result.finish_reason.as_str()),
                            Some(&run_result),
                            run_result.output.as_deref(),
                            run_result.error.as_deref(),
                        )
                        .await;
                    let duration_ms = started_at
                        .elapsed()
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    spawn_run_result_to_sync_output(agent_id_for_output, run_result, duration_ms)
                }
                Err(error) => {
                    spawner
                        .finalize_background_agent(
                            &agent_id_for_task,
                            AgentStatus::Failed {
                                error: error.clone(),
                                finish_reason: None,
                            },
                            "failed",
                            None,
                            None,
                            None,
                            Some(error.as_str()),
                        )
                        .await;
                    SpawnAgentOutput::Failed {
                        error,
                        duration_ms: 0,
                    }
                }
            };
            let _ = terminal_tx.send(output);
            notify_after_completion.notify_waiters();
            std::mem::forget(guard);
        };
        let abort_handle = self
            .background_tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .spawn(spawn_future);
        self.background_abort_handles
            .write()
            .await
            .insert(agent_id.clone(), abort_handle);

        if input.run_in_background {
            self.remember_background_agent_id(&agent_id);
            return Ok(SpawnAgentOutput::Launched {
                agent_id,
                description,
                messaging_address: messaging_address_text,
            });
        }

        loop {
            if self.take_foreground_promotion_request(&agent_id).await {
                return Ok(SpawnAgentOutput::Launched {
                    agent_id,
                    description,
                    messaging_address: messaging_address_text,
                });
            }

            tokio::select! {
                terminal = &mut terminal_rx => {
                    // Remove abort handle for foreground agents that completed
                    // synchronously. The spawn_future also calls
                    // finalize_background_agent which removes it, but this explicit
                    // removal guarantees cleanup even if the spawn_future panics
                    // or the JoinSet task is dropped before finalization.
                    self.background_abort_handles.write().await.remove(&agent_id);
                    self.reap_finished_agent_tasks();
                    return Ok(terminal.unwrap_or_else(|_| SpawnAgentOutput::Failed {
                        error: "agent task ended before returning a result".to_string(),
                        duration_ms: 0,
                    }));
                }
                _ = notify.notified() => {}
            }
        }
    }

    /// Cancel a single background agent by id. Returns true only when this call
    /// actually owned the cancellation and archived the agent as cancelled.
    pub async fn cancel_agent(&self, agent_id: &str, reason: &str) -> bool {
        // Single write-lock scope that *atomically* removes both the abort
        // handle and the agent state.  This prevents a TOCTOU race where the
        // monitor finalises the agent between handle removal and state
        // finalization — which would leave the cancel_agent caller thinking
        // it "won" while the monitor already cleaned up (and possibly recorded
        // a different terminal status for fanout slots).
        let (abort_handle, mut state) = {
            let mut handles = self.background_abort_handles.write().await;
            let Some(handle) = handles.remove(agent_id) else {
                return false;
            };
            if handle.is_finished() {
                return false;
            }
            // Atomically seize the active state.  If we hold the abort handle
            // but not the state, the monitor may finalize independently and
            // leave fanout slots in a non-terminal state (or worse, in a
            // terminal state that disagrees with user intent).
            let mut active = self.active_agents.write().await;
            let Some(state) = active.remove(agent_id) else {
                return false;
            };
            (handle, state)
        };

        abort_handle.abort();
        // `cancel_agent` is the user-driven cancel surface (Ctrl+G x,
        // /agent cancel, etc.). Propagate the user-driven flag so the
        // wire output tells the LLM not to respawn.
        self.finalize_cancelled_agent(&mut state, agent_id, reason)
            .await
    }

    /// Finalize an agent that was atomically seized by [`cancel_agent`].
    /// Performs all the same cleanup as [`finalize_background_agent`] but
    /// operates on a pre-extracted [`SpawnedAgentState`] — the caller already
    /// owns the state and the abort handle is already removed from the book.
    async fn finalize_cancelled_agent(
        &self,
        state: &mut SpawnedAgentState,
        agent_id: &str,
        reason: &str,
    ) -> bool {
        self.remove_background_agent_id(agent_id);

        let status = AgentStatus::cancelled_by_user(reason);
        state.status = status;
        let messaging_address = state.messaging_address.take();

        self.record_fanout_terminal_state(state).await;
        self.emit_agent_terminal_trace(state, "cancelled", Some(reason), Some(reason), None)
            .await;
        self.persist_agent_terminated_state(state, "cancelled", Some(reason))
            .await;
        if let Some(event_type) =
            agent_status_to_progress_event(&state.status, &state.metrics, state.started_at)
        {
            let timestamp_epoch_ms = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            self.progress_broadcaster.emit(AgentProgressEvent {
                agent_id: agent_id.to_string(),
                event_type,
                timestamp_epoch_ms,
            });
        }
        if let Some(addr) = messaging_address
            && let Err(err) = self.mailbox_router.unregister(&addr).await
        {
            eprintln!(
                "  ⚠ messaging: failed to unregister mailbox for '{}': {}",
                agent_id, err
            );
        }
        self.archive_state(state.clone()).await;
        self.notify_completion(agent_id).await;
        true
    }

    async fn finalize_background_agent(
        &self,
        agent_id: &str,
        status: AgentStatus,
        journal_status: &str,
        finish_reason: Option<&str>,
        run_result: Option<&SpawnRunResult>,
        output: Option<&str>,
        error: Option<&str>,
    ) -> bool {
        self.background_abort_handles.write().await.remove(agent_id);
        self.remove_background_agent_id(agent_id);
        let (state, messaging_address) = {
            let mut active_agents = self.active_agents.write().await;
            let Some(mut state) = active_agents.remove(agent_id) else {
                return false;
            };
            if let Some(run_result) = run_result {
                state.metrics.tool_calls = run_result.tool_calls;
                state.metrics.prompt_tokens = run_result.prompt_tokens;
                state.metrics.completion_tokens = run_result.completion_tokens;
                state.metrics.permission_requests = run_result.permission_requests;
                state.metrics.permission_requests_approved =
                    run_result.permission_requests_approved;
                state.metrics.tools_blocked = run_result.tools_blocked;
                if let Some(summary) = run_result.permission_summary.clone() {
                    state.permission_summary = summary;
                }
            }
            state.status = status;
            let messaging_address = state.messaging_address.take();
            (state, messaging_address)
        };

        self.record_fanout_terminal_state(&state).await;
        self.emit_agent_terminal_trace(&state, journal_status, finish_reason, output, error)
            .await;
        self.persist_agent_terminated_state(&state, journal_status, finish_reason)
            .await;
        if let Some(event_type) =
            agent_status_to_progress_event(&state.status, &state.metrics, state.started_at)
        {
            let timestamp_epoch_ms = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            self.progress_broadcaster.emit(AgentProgressEvent {
                agent_id: agent_id.to_string(),
                event_type,
                timestamp_epoch_ms,
            });
        }
        if let Some(addr) = messaging_address
            && let Err(err) = self.mailbox_router.unregister(&addr).await
        {
            eprintln!(
                "  ⚠ messaging: failed to unregister mailbox for '{}': {}",
                agent_id, err
            );
        }
        self.archive_state(state).await;
        self.notify_completion(agent_id).await;
        true
    }

    fn remove_background_agent_id(&self, agent_id: &str) {
        let mut ids = self
            .background_agent_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ids.retain(|id| id != agent_id);
    }

    /// Persist final agent state to session journal (best-effort).
    async fn persist_agent_terminated_state(
        &self,
        state: &SpawnedAgentState,
        status: &str,
        finish_reason: Option<&str>,
    ) {
        let Some(ref sid) = self.session_id else {
            return;
        };
        let writer = match astra_services::session_journal::JournalWriter::new(sid) {
            Ok(w) => w,
            Err(e) => {
                astra_core::agent_warn!(
                    "spawner",
                    "journal writer init failed for {}: {e}",
                    state.agent_id
                );
                return;
            }
        };
        let duration_ms = state
            .started_at
            .elapsed()
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let event = astra_services::session_journal::JournalEvent::agent_terminated(
            Some(sid.as_str()),
            &state.agent_id,
            &state.run_id,
            &state.agent_type,
            status,
            finish_reason,
            state.metrics.turns_completed,
            state.metrics.tool_calls,
            state.metrics.prompt_tokens,
            state.metrics.completion_tokens,
            duration_ms,
        );
        if let Err(e) = writer.append(&event) {
            astra_core::agent_warn!(
                "spawner",
                "journal append failed for {}: {e}",
                state.agent_id
            );
        }
    }

    async fn archive_state(&self, state: SpawnedAgentState) {
        let mut completed = self.completed_agents.write().await;
        const MAX_COMPLETED_AGENTS: usize = 256;
        if completed.len() >= MAX_COMPLETED_AGENTS {
            completed.remove(0);
        }
        completed.push(state);
    }

    async fn notify_completion(&self, agent_id: &str) {
        if let Some(notifier) = self.completion_notifiers.write().await.remove(agent_id) {
            notifier.notify_waiters();
        }
    }

    fn agent_status_trace_label(status: &AgentStatus) -> &'static str {
        match status {
            AgentStatus::Initializing => "initializing",
            AgentStatus::Running { .. } => "running",
            AgentStatus::Idle => "idle",
            AgentStatus::Completed { .. } => "completed",
            AgentStatus::Failed { .. } => "failed",
            AgentStatus::Cancelled { .. } => "cancelled",
        }
    }

    pub async fn record_agent_result_collected(
        &self,
        parent_run_id: &str,
        parent_agent_id: &str,
        agent_id: &str,
        tool_call_id: Option<&str>,
        child_status: &AgentStatus,
    ) {
        let state = self.get_agent_state_any(agent_id).await;
        let Some(state) = state else {
            return;
        };
        self.mark_fanout_result_collected(&state).await;
        let Some(trace) = state.trace_context.as_ref() else {
            return;
        };
        let tool_key = tool_call_id.unwrap_or("");
        let mut event = TraceEvent::new(
            Self::trace_event_id("agent_collect", &[parent_run_id, tool_key, agent_id]),
            trace.session_id.clone(),
            trace.user_id.clone(),
            "agent_result_collected",
            "agent_lifecycle",
        )
        .with_turn_context(trace);
        event.run_id = Some(parent_run_id.to_string());
        event.parent_run_id = None;
        event.agent_id = Some(parent_agent_id.to_string());
        event.parent_agent_id = None;
        event.tool_call_id = tool_call_id.map(ToString::to_string);
        event.parent_event_id = Some(trace.root_event_id.clone());
        event.metadata = serde_json::json!({
            "child_agent_id": agent_id,
            "child_run_id": &state.run_id,
            "child_status": Self::agent_status_trace_label(child_status),
        });
        self.write_trace_event(event).await;
    }

    /// Wait for a background agent to complete. Returns immediately if
    /// already completed. Uses `tokio::sync::Notify` — zero polling.
    pub async fn wait_for_agent(
        &self,
        agent_id: &str,
        timeout: std::time::Duration,
    ) -> Option<AgentStatus> {
        match self.wait_for_agent_outcome(agent_id, timeout).await {
            WaitForAgentOutcome::Status(status) => Some(status),
            WaitForAgentOutcome::TimedOut | WaitForAgentOutcome::Unknown => None,
        }
    }

    /// Classify a wait that did NOT find a terminal status: if the agent is
    /// still tracked anywhere we treat it as a timeout; otherwise the caller
    /// gave us an id that was never registered.
    async fn classify_wait_failure(&self, agent_id: &str) -> WaitForAgentOutcome {
        if self.get_agent_state_any(agent_id).await.is_some() {
            WaitForAgentOutcome::TimedOut
        } else {
            WaitForAgentOutcome::Unknown
        }
    }

    /// Wait for a background agent to complete and distinguish timeout from
    /// "unknown id" in the same code path that owns notifier registration.
    pub async fn wait_for_agent_outcome(
        &self,
        agent_id: &str,
        timeout: std::time::Duration,
    ) -> WaitForAgentOutcome {
        // Already completed?
        for s in self.completed_agents.read().await.iter() {
            if s.agent_id == agent_id {
                return WaitForAgentOutcome::Status(s.status.clone());
            }
        }

        // Get notifier and create the future BEFORE re-checking, so a
        // notification that fires between the first check and .notified()
        // is not lost.
        let notifier = {
            let map = self.completion_notifiers.read().await;
            map.get(agent_id).cloned()
        };
        let Some(notifier) = notifier else {
            return self.classify_wait_failure(agent_id).await;
        };
        let notify_future = notifier.notified();
        tokio::pin!(notify_future);
        notify_future.as_mut().enable();

        // Re-check after registering the future — catches completions
        // that raced between the first check and notified() registration.
        for s in self.completed_agents.read().await.iter() {
            if s.agent_id == agent_id {
                return WaitForAgentOutcome::Status(s.status.clone());
            }
        }

        // Wait for notification with timeout. Whether the notifier fired
        // (Ok) or the timeout elapsed (Err), the same recovery applies:
        // re-check completed_agents (the notifier may have woken us
        // before handle_completion finished writing) and otherwise fall
        // through to classify_wait_failure.
        let _ = tokio::time::timeout(timeout, notify_future).await;
        for s in self.completed_agents.read().await.iter() {
            if s.agent_id == agent_id {
                return WaitForAgentOutcome::Status(s.status.clone());
            }
        }
        self.classify_wait_failure(agent_id).await
    }

    /// Clone the spawner for use in spawned tasks.
    fn clone_for_task(&self) -> Self {
        Self {
            mailbox_router: Arc::clone(&self.mailbox_router),
            active_agents: Arc::clone(&self.active_agents),
            progress_broadcaster: Arc::clone(&self.progress_broadcaster),
            context_cache: Arc::clone(&self.context_cache),
            executor: self.executor.clone(),
            session_id: self.session_id.clone(),
            agent_registry: self.agent_registry.clone(),
            completed_agents: Arc::clone(&self.completed_agents),
            // Share the same JoinSet so shutdown can drain tasks spawned by clones.
            background_tasks: Arc::clone(&self.background_tasks),
            background_abort_handles: Arc::clone(&self.background_abort_handles),
            background_agent_ids: Arc::clone(&self.background_agent_ids),
            completion_notifiers: Arc::clone(&self.completion_notifiers),
            foreground_promotion_requests: Arc::clone(&self.foreground_promotion_requests),
            // Share prefix-store + resolve-outcomes map so clones
            // see/write the same view. The store is an Arc<dyn ...>
            // itself already, so cloning the Option just bumps refcount.
            prefix_store: self.prefix_store.clone(),
            prefix_resolve_outcomes: Arc::clone(&self.prefix_resolve_outcomes),
            trace_writer: self.trace_writer.clone(),
            max_concurrent_agents: self.max_concurrent_agents,
            fanout_groups: Arc::clone(&self.fanout_groups),
        }
    }

    /// Drain all background agents, wait up to `deadline`, and return
    /// completed results so the caller can surface them.
    ///
    /// Returns `(agent_id, result_text)` for every background child that
    /// finished with `AgentStatus::Completed`. Aborts tasks that exceed
    /// `deadline`; panics inside a background task are caught and logged.
    pub async fn shutdown_and_wait(&self, deadline: std::time::Duration) -> Vec<(String, String)> {
        let mut set = self
            .background_tasks
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_else(|poisoned| {
                let mut guard = poisoned.into_inner();
                std::mem::take(&mut *guard)
            });

        // Drain JoinSet — even if empty (tasks may have already completed).
        match tokio::time::timeout(deadline, async {
            while let Some(result) = set.join_next().await {
                if let Err(e) = result {
                    if e.is_panic() {
                        astra_core::agent_warn!(
                            "spawner",
                            "background agent task panicked during shutdown drain"
                        );
                    }
                }
            }
        })
        .await
        {
            Ok(()) => {}
            Err(_) => {
                astra_core::agent_warn!(
                    "spawner",
                    "background agent drain timed out after {deadline:?}; aborting remaining tasks"
                );
                set.abort_all();
            }
        }

        // Clean up any leftover completion notifiers (e.g. from timed-out tasks).
        self.background_abort_handles.write().await.clear();
        self.completion_notifiers.write().await.clear();

        self.completed_agents
            .read()
            .await
            .iter()
            .filter(|s| s.run_in_background)
            .filter_map(|s| {
                if let AgentStatus::Completed { ref result, .. } = s.status {
                    Some((s.agent_id.clone(), result.clone()))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Number of in-flight background tasks currently tracked.
    /// Primarily useful for tests and observability.
    pub fn background_task_count(&self) -> usize {
        self.background_tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// List all active agents spawned by a parent.
    pub async fn list_agents(&self, parent_run_id: &str) -> Vec<SpawnedAgentInfo> {
        self.active_agents
            .read()
            .await
            .values()
            .filter(|s| s.parent_run_id == parent_run_id)
            .map(SpawnedAgentInfo::from)
            .collect()
    }

    /// List all agents (no filter).
    pub async fn list_all_agents(&self) -> Vec<SpawnedAgentInfo> {
        self.active_agents
            .read()
            .await
            .values()
            .map(SpawnedAgentInfo::from)
            .collect()
    }

    /// Get state of a specific agent.
    pub async fn get_agent_state(&self, agent_id: &str) -> Option<SpawnedAgentState> {
        self.active_agents.read().await.get(agent_id).cloned()
    }

    /// Get state of a specific agent, including archived completed agents.
    pub async fn get_agent_state_any(&self, agent_id: &str) -> Option<SpawnedAgentState> {
        if let Some(state) = self.active_agents.read().await.get(agent_id).cloned() {
            return Some(state);
        }
        self.completed_agents
            .read()
            .await
            .iter()
            .find(|state| state.agent_id == agent_id)
            .cloned()
    }

    /// Get history of completed agents (both active and archived).
    pub async fn get_agent_history(&self, parent_run_id: Option<&str>) -> Vec<SpawnedAgentInfo> {
        let mut history: Vec<SpawnedAgentInfo> = self
            .completed_agents
            .read()
            .await
            .iter()
            .filter(|s| parent_run_id.is_none_or(|pid| s.parent_run_id == pid))
            .map(SpawnedAgentInfo::from)
            .collect();
        // Also include still-active agents.
        for state in self.active_agents.read().await.values() {
            if parent_run_id.is_none_or(|pid| state.parent_run_id == pid) {
                if !history.iter().any(|h| h.agent_id == state.agent_id) {
                    history.push(SpawnedAgentInfo::from(state));
                }
            }
        }
        history
    }

    /// Update agent status.
    pub async fn update_status(&self, agent_id: &str, status: AgentStatus) {
        if let Some(state) = self.active_agents.write().await.get_mut(agent_id) {
            state.status = status.clone();

            let Some(event_type) =
                agent_status_to_progress_event(&status, &state.metrics, state.started_at)
            else {
                return;
            };

            let timestamp_epoch_ms = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            self.progress_broadcaster.emit(AgentProgressEvent {
                agent_id: agent_id.to_string(),
                event_type,
                timestamp_epoch_ms,
            });
        }
    }

    /// Subscribe to progress events for all spawned agents.
    pub fn subscribe_progress(&self) -> tokio::sync::broadcast::Receiver<AgentProgressEvent> {
        self.progress_broadcaster.subscribe()
    }

    /// Get a reference to the progress broadcaster.
    pub fn progress_broadcaster(&self) -> Arc<ProgressBroadcaster> {
        Arc::clone(&self.progress_broadcaster)
    }
}

/// Historical record of a terminated agent, reconstructed from journal.
#[derive(Debug, Clone)]
pub struct AgentHistoryRecord {
    pub agent_id: String,
    pub run_id: String,
    pub agent_type: String,
    pub status: String,
    pub turns_completed: u32,
    pub tool_calls: u32,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub duration_ms: u64,
    pub timestamp: String,
}

// ─── Errors ─────────────────────────────────────────────────────────────────

/// Errors that can occur during agent spawning.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("Invalid spawn input: {0}")]
    InvalidInput(String),

    #[error("Unknown agent type: {0}")]
    UnknownAgentType(String),

    #[error("Recursion depth limit exceeded: {0}")]
    DepthLimitExceeded(String),

    #[error("Mailbox registration failed: {0}")]
    MailboxRegistration(String),

    #[error("Worktree creation failed: {0}")]
    WorktreeCreation(String),

    #[error("Delegation failed: {0}")]
    DelegationFailed(String),

    /// Fork children are allowed to spawn normal children, but not
    /// another inherit-prefix fork. This mirrors Claude Code's
    /// `isInForkChild()` guard and prevents recursive cache-key drift.
    #[error("Nested fork inheritance is not allowed from a fork child")]
    NestedForkInheritanceRejected,

    /// Fired when `inherit_prefix.required=true` but the resolver
    /// could not attach a matching prefix (missing, incompatible,
    /// or feature-disabled). Soft failures (`required=false`)
    /// produce no error; they fall back to a fresh spawn and are
    /// only visible via `last_prefix_resolve`.
    #[error("Required prefix inheritance failed: {reason}")]
    PrefixInheritanceRequired { reason: String },

    /// Agent was cancelled or removed between reservation and spawn
    /// completion. This is a transient race, not a user error — the
    /// caller should treat it as a soft failure (same as a cancelled
    /// spawn) rather than propagating a hard error.
    #[error("Spawn race: {0}")]
    Race(String),

    /// Concurrency cap reached. The LLM (or human caller) should wait
    /// for an outstanding agent to finish, cancel one explicitly, or
    /// retry. Carries the live counts so the caller's error message
    /// can be specific.
    #[error(
        "Concurrent agent cap reached: {active} active, limit {limit}. \
         Wait for an existing agent to finish or cancel one before spawning more."
    )]
    ConcurrencyLimitExceeded { active: usize, limit: usize },
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Build the [`InheritedChildPrefix`] payload handed to the executor.
///
/// Only produces `Some` when `resolve_outcome` is `Resolved`. On
/// reconstruct error (corrupt canonical bytes — rare, implies the
/// captured prefix was mangled in-flight) we degrade to `None`: the
/// child runs fresh, same as the non-resolved path. A future
/// telemetry layer (PR 5.6+) may want to emit a structured
/// "reconstruct failed" event here; currently the failure is silent
/// because no sink is wired through at this layer.
///
/// Visibility: `pub(crate)` so `server::delegation::engine` can
/// share the same helper — delegate path wires fork-prefix exactly
/// the same way the spawner does (Bug B step 2).
pub(crate) fn build_inherited_child_prefix(
    resolve_outcome: &PrefixResolveOutcome,
) -> Option<InheritedChildPrefix> {
    let prefix = match resolve_outcome {
        PrefixResolveOutcome::Resolved { prefix } => prefix,
        _ => return None,
    };
    // reconstruct_messages ONLY fails when the captured canonical
    // bytes are not a JSON array of message objects. That would
    // imply a corrupt capture — extremely rare in practice. We
    // degrade to None rather than failing the spawn, so the child
    // runs fresh (equivalent to the resolver's soft-fallback path).
    // A future telemetry layer (PR 5.6+) may want to emit a
    // "reconstruct failed" event here; currently no sink is wired
    // through so the failure is silent.
    match reconstruct_messages(prefix, Vec::new()) {
        Ok(r) => {
            let frozen_tools: Option<Vec<serde_json::Value>> = {
                let entries = prefix.tool_schemas();
                if entries.is_empty() {
                    None
                } else {
                    let parsed: Vec<serde_json::Value> = entries
                        .iter()
                        .filter_map(|e| serde_json::from_slice(&e.canonical_bytes).ok())
                        .collect();
                    if parsed.is_empty() {
                        None
                    } else {
                        Some(parsed)
                    }
                }
            };
            Some(InheritedChildPrefix {
                prefix_id: prefix.prefix_id.clone(),
                parent_run_id: prefix.parent_run_id.clone(),
                provider: prefix.provider.clone(),
                thinking: prefix.thinking.clone(),
                prefix_messages: r.messages,
                frozen_tool_schemas: frozen_tools,
                expected_cache_read_tokens: estimate_cache_read_tokens(prefix),
            })
        }
        Err(_) => None,
    }
}

fn estimate_cache_read_tokens(prefix: &astra_turn_core::fork_prefix::ForkPrefix) -> u64 {
    fn bytes_to_tokens(bytes: usize) -> u64 {
        // Conservative, provider-neutral approximation: four bytes per
        // token, rounded up. The probe only needs a nonzero baseline
        // good enough to distinguish full misses from useful reuse.
        u64::try_from(bytes.div_ceil(4)).unwrap_or(u64::MAX)
    }

    // `size_bytes()` is already the canonical serialized prefix region
    // (system + tools + messages). Re-adding system/tool payload sizes
    // would double-count those bytes and systematically understate the
    // observed/expected cache-hit ratio in telemetry.
    bytes_to_tokens(prefix.size_bytes()).max(1)
}

/// Build permission summary from spawn context.
fn build_permission_summary(context: &SpawnContext) -> PermissionSummary {
    let mut summary = PermissionSummary::default();

    if let Some(ref inherited) = context.inherited_permissions {
        summary.mode = match inherited.mode {
            super::permission_sync::PermissionMode::Auto => "auto".to_string(),
            super::permission_sync::PermissionMode::Plan => "plan".to_string(),
            super::permission_sync::PermissionMode::AcceptEdits => "accept_edits".to_string(),
            super::permission_sync::PermissionMode::Prompt => "prompt".to_string(),
            super::permission_sync::PermissionMode::Deny => "deny".to_string(),
        };
        summary.allow_rules = inherited.allow_rules.len() as u32;
        summary.deny_rules = inherited.deny_rules.len() as u32;
        // Has parent if parent_run_id is not empty and not "root"
        summary.has_parent =
            !context.parent_run_id.is_empty() && context.parent_run_id != ROOT_RUN_ID;
    } else {
        summary.mode = "auto".to_string();
        summary.has_parent =
            !context.parent_run_id.is_empty() && context.parent_run_id != ROOT_RUN_ID;
    }

    summary
}

/// Create an isolated git worktree for a spawned agent.
///
/// Creates `<parent_dir>/.agent-worktrees/<run_id>` via `git worktree add`.
/// Returns the path on success. Falls back to a simple directory copy if
/// the parent directory is not a git repo.
fn create_agent_worktree(parent_dir: &std::path::Path, run_id: &str) -> Result<PathBuf, String> {
    let worktree_base = parent_dir.join(".agent-worktrees");
    std::fs::create_dir_all(&worktree_base)
        .map_err(|e| format!("cannot create worktree base: {e}"))?;

    let worktree_path = worktree_base.join(run_id);

    // Try git worktree first
    let output = std::process::Command::new("git")
        .args(["worktree", "add", "--detach"])
        .arg(&worktree_path)
        .arg("HEAD")
        .current_dir(parent_dir)
        .output()
        .map_err(|e| format!("git worktree exec failed: {e}"))?;

    if output.status.success() {
        return Ok(worktree_path);
    }

    // Fallback: create an empty working directory (non-git isolation)
    std::fs::create_dir_all(&worktree_path)
        .map_err(|e| format!("cannot create worktree dir: {e}"))?;
    Ok(worktree_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::delegation::engine::DelegationTracker;
    use astra_messaging::in_process::InProcessTransport;
    use astra_messaging::router::AgentMailboxRouter;
    use astra_messaging::types::{AgentMessage, MessagePayload, MessageTarget};
    use tokio::time::{Duration, sleep};

    fn mock_router() -> Arc<AgentMailboxRouter> {
        let transport = Arc::new(InProcessTransport::new());
        let dt = Arc::new(DelegationTracker::new());
        Arc::new(AgentMailboxRouter::new(transport, dt))
    }

    #[tokio::test]
    async fn test_spawn_basic() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let input = SpawnAgentInput {
            description: "Test agent".to_string(),
            prompt: "Do a test".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: true,
            ..Default::default()
        };
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "parent".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: None,
            inherited_skills: vec![],
            live_event_sink: None,
            trace_context: None,
            spawn_tool_call_id: None,
        };

        let result = spawner.spawn(input, &context).await.unwrap();
        assert!(matches!(result, SpawnAgentOutput::Launched { .. }));
    }

    #[tokio::test]
    async fn test_unknown_agent_type() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let input = SpawnAgentInput {
            description: "Test".to_string(),
            prompt: "Test".to_string(),
            agent_type: "unknown-type".to_string(),
            ..Default::default()
        };
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "parent".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: None,
            inherited_skills: vec![],
            live_event_sink: None,
            trace_context: None,
            spawn_tool_call_id: None,
        };

        let result = spawner.spawn(input, &context).await;
        assert!(matches!(result, Err(SpawnError::UnknownAgentType(_))));
    }

    #[test]
    fn agent_status_to_progress_event_keeps_interrupted_completion_distinct() {
        let event = agent_status_to_progress_event(
            &AgentStatus::Completed {
                result: "partial".to_string(),
                finish_reason: Some("budget_exhausted".to_string()),
            },
            &SpawnedAgentMetrics {
                tool_calls: 2,
                prompt_tokens: 10,
                completion_tokens: 5,
                ..Default::default()
            },
            SystemTime::now(),
        )
        .expect("completed status should emit progress");

        assert!(matches!(
            event,
            ProgressEventType::Interrupted {
                reason,
                partial_summary,
                total_tool_calls: 2,
                total_tokens: (10, 5),
                ..
            } if reason == "budget_exhausted" && partial_summary == "partial"
        ));
    }

    #[tokio::test]
    async fn test_list_agents() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "parent".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: None,
            inherited_skills: vec![],
            live_event_sink: None,
            trace_context: None,
            spawn_tool_call_id: None,
        };

        // Spawn two agents
        for i in 0..2 {
            let input = SpawnAgentInput {
                description: format!("Agent {}", i),
                prompt: "Test".to_string(),
                agent_type: "explore".to_string(),
                ..Default::default()
            };
            let _ = spawner.spawn(input, &context).await;
        }

        let agents = spawner.list_agents("parent-123").await;
        assert_eq!(agents.len(), 2);
    }

    #[tokio::test]
    async fn test_context_cache_shared_across_spawns() {
        use astra_turn_core::orchestration_context_cache::SharedContextCache;

        // Create a shared context cache
        let cache = Arc::new(SharedContextCache::default());

        // Create spawner with custom cache
        let spawner = DynamicAgentSpawner::with_context_cache(mock_router(), Arc::clone(&cache));

        // Verify spawner has the same cache
        assert!(Arc::ptr_eq(&cache, spawner.context_cache()));

        // Parent agent stores some knowledge
        cache.share_knowledge(
            "project/tech-stack",
            serde_json::json!({"db": "postgres"}),
            "parent-agent",
        );

        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "parent".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: None,
            inherited_skills: vec![],
            live_event_sink: None,
            trace_context: None,
            spawn_tool_call_id: None,
        };

        // Spawn an agent in background mode
        let input = SpawnAgentInput {
            description: "Explore codebase".to_string(),
            prompt: "Explore".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: true,
            ..Default::default()
        };
        let result = spawner.spawn(input, &context).await.unwrap();
        assert!(matches!(result, SpawnAgentOutput::Launched { .. }));

        // The cache still has the knowledge from parent
        let knowledge = cache.get_knowledge("project/tech-stack");
        assert!(knowledge.is_some());
        assert_eq!(knowledge.unwrap()["db"], "postgres");

        // Spawned agent can also add knowledge (simulated)
        cache.share_knowledge(
            "project/auth",
            serde_json::json!({"type": "jwt"}),
            "spawned-agent",
        );

        // All knowledge is accessible
        assert_eq!(cache.knowledge_count(), 2);
    }

    #[tokio::test]
    async fn test_named_spawn_records_parent_routing() {
        let router = mock_router();
        let spawner = DynamicAgentSpawner::new(router.clone());
        let mut parent_mailbox = router
            .register(AgentAddress::new("parent-123", "main"), None)
            .await
            .unwrap();
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "main".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            inherited_permissions: None,
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
            live_event_sink: None,
            trace_context: None,
            spawn_tool_call_id: None,
        };
        let input = SpawnAgentInput {
            description: "Named agent".to_string(),
            prompt: "Send a message".to_string(),
            agent_type: "explore".to_string(),
            name: Some("named".to_string()),
            run_in_background: true,
            ..Default::default()
        };

        let agent_id = match spawner.spawn(input, &context).await.unwrap() {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched output, got {other:?}"),
        };
        let state = spawner.get_agent_state(&agent_id).await.unwrap();
        let child_addr = state
            .messaging_address
            .expect("named agent should have mailbox");

        router
            .send(AgentMessage::new(
                child_addr,
                MessageTarget::Parent,
                MessagePayload::Text {
                    content: "done".into(),
                    summary: None,
                },
            ))
            .await
            .unwrap();

        let received = parent_mailbox
            .try_recv()
            .expect("parent should receive message");
        match &received.payload {
            MessagePayload::Text { content, .. } => assert_eq!(content, "done"),
            other => panic!("expected text payload, got {other:?}"),
        }
    }

    struct ImmediateSuccessExecutor;

    struct ImmediateStatusExecutor {
        status: &'static str,
        finish_reason: &'static str,
        output: Option<&'static str>,
        error: Option<&'static str>,
    }

    struct CapturingDepthExecutor {
        captured_depth: std::sync::Mutex<Option<u8>>,
    }

    impl CapturingDepthExecutor {
        fn new() -> Self {
            Self {
                captured_depth: std::sync::Mutex::new(None),
            }
        }
    }

    /// Executor that captures the `inherited_prefix` field from the
    /// SpawnRunConfig so PR 5.5 tests can assert the spawner builds
    /// it correctly from the resolver outcome.
    struct CapturingPrefixExecutor {
        captured: std::sync::Mutex<Option<Option<InheritedChildPrefix>>>,
    }

    impl CapturingPrefixExecutor {
        fn new() -> Self {
            Self {
                captured: std::sync::Mutex::new(None),
            }
        }
        fn take_captured(&self) -> Option<Option<InheritedChildPrefix>> {
            self.captured.lock().unwrap().take()
        }
    }

    #[async_trait]
    impl SpawnAgentExecutor for CapturingPrefixExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            *self.captured.lock().unwrap() = Some(config.inherited_prefix.clone());
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "completed".into(),
                finish_reason: "normal".into(),
                output: Some("ok".into()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
                permission_summary: None,
                permission_requests: 0,
                permission_requests_approved: 0,
                tools_blocked: 0,
            })
        }
    }

    #[async_trait]
    impl SpawnAgentExecutor for ImmediateSuccessExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "completed".into(),
                finish_reason: "normal".into(),
                output: Some("ok".into()),
                error: None,
                prompt_tokens: 1,
                completion_tokens: 1,
                tool_calls: 0,
                permission_summary: None,
                permission_requests: 0,
                permission_requests_approved: 0,
                tools_blocked: 0,
            })
        }
    }

    #[async_trait]
    impl SpawnAgentExecutor for ImmediateStatusExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: self.status.into(),
                finish_reason: self.finish_reason.into(),
                output: self.output.map(str::to_string),
                error: self.error.map(str::to_string),
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
                permission_summary: None,
                permission_requests: 0,
                permission_requests_approved: 0,
                tools_blocked: 0,
            })
        }
    }

    #[async_trait]
    impl SpawnAgentExecutor for CapturingDepthExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            *self.captured_depth.lock().unwrap() = Some(config.recursion_depth);
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "completed".into(),
                finish_reason: "normal".into(),
                output: Some("ok".into()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
                permission_summary: None,
                permission_requests: 0,
                permission_requests_approved: 0,
                tools_blocked: 0,
            })
        }
    }

    #[test]
    fn spawn_run_status_helpers_distinguish_interrupted_and_fail_closed_unknown() {
        assert_eq!(
            spawn_run_status_kind(SPAWN_STATUS_INTERRUPTED),
            SpawnRunStatusKind::Interrupted
        );
        assert_eq!(
            spawn_run_status_kind(SPAWN_STATUS_COMPLETED),
            SpawnRunStatusKind::Completed
        );
        assert_eq!(spawn_run_status_kind("mystery"), SpawnRunStatusKind::Other);

        let interrupted = SpawnRunResult {
            agent_id: "a1".into(),
            run_id: "r1".into(),
            status: SPAWN_STATUS_INTERRUPTED.into(),
            finish_reason: "budget_exhausted".into(),
            output: Some("partial".into()),
            error: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
            permission_summary: None,
            permission_requests: 0,
            permission_requests_approved: 0,
            tools_blocked: 0,
        };
        assert!(matches!(
            spawn_run_result_to_agent_status(&interrupted),
            AgentStatus::Completed { .. }
        ));
        assert!(matches!(
            spawn_run_result_to_sync_output("a1".into(), interrupted.clone(), 12),
            SpawnAgentOutput::Interrupted {
                finish_reason,
                result,
                ..
            } if finish_reason == "budget_exhausted" && result == "partial"
        ));

        let unknown = SpawnRunResult {
            status: "mystery".into(),
            finish_reason: "unknown".into(),
            ..interrupted.clone()
        };
        assert!(matches!(
            spawn_run_result_to_agent_status(&unknown),
            AgentStatus::Failed { .. }
        ));
        assert!(matches!(
            spawn_run_result_to_sync_output("a1".into(), unknown, 12),
            SpawnAgentOutput::Failed { .. }
        ));
    }

    #[test]
    fn fanout_slot_status_counts_only_budget_interruptions_as_parent_budget_cancel() {
        let budget_interrupted = AgentStatus::Completed {
            result: "partial review".to_string(),
            finish_reason: Some("budget_exhausted".to_string()),
        };
        let (status, reason) = fanout_slot_status_from_agent_status(&budget_interrupted);
        assert_eq!(status, AgentFanoutSlotStatus::CancelledByParentBudget);
        assert_eq!(reason.as_deref(), Some("budget_exhausted"));

        let empty_completion = AgentStatus::Completed {
            result: String::new(),
            finish_reason: Some("empty_completion".to_string()),
        };
        let (status, reason) = fanout_slot_status_from_agent_status(&empty_completion);
        assert_eq!(
            status,
            AgentFanoutSlotStatus::Failed,
            "non-budget interrupted completions are failed slots, not parent-budget cancellations"
        );
        assert_eq!(reason.as_deref(), Some("empty_completion"));
    }

    #[test]
    fn spawn_completion_status_tracks_interrupted_finish_reasons() {
        assert_eq!(
            spawn_completion_status_from_finish_reason(None),
            "completed"
        );
        assert_eq!(
            spawn_completion_status_from_finish_reason(Some("normal")),
            "completed"
        );
        assert_eq!(
            spawn_completion_status_from_finish_reason(Some("budget_exhausted")),
            "interrupted"
        );
        assert_eq!(
            spawn_completion_status_from_finish_reason(Some("context_overflow")),
            "interrupted"
        );
        assert_eq!(
            spawn_completion_status_from_finish_reason(Some("empty_completion")),
            "interrupted"
        );
    }

    #[test]
    fn subrun_status_projection_maps_paused_and_unknown_via_spawn_owner() {
        let paused = project_subrun_status_to_spawn(astra_core::STATUS_PAUSED, None);
        assert_eq!(paused.status, SPAWN_STATUS_INTERRUPTED);
        assert_eq!(paused.finish_reason, SPAWN_STATUS_INTERRUPTED);
        assert!(paused.error.is_none());

        let unknown = project_subrun_status_to_spawn("mystery", None);
        assert_eq!(unknown.status, SPAWN_STATUS_FAILED);
        assert_eq!(unknown.finish_reason, "unknown");
        assert_eq!(
            unknown.error.as_deref(),
            Some("server spawned agent ended with mystery")
        );
    }

    #[test]
    fn subrun_status_projection_preserves_explicit_failure_detail() {
        let failed =
            project_subrun_status_to_spawn(astra_core::STATUS_FAILED, Some("boom".to_string()));
        assert_eq!(failed.status, SPAWN_STATUS_FAILED);
        assert_eq!(failed.finish_reason, "failed");
        assert_eq!(failed.error.as_deref(), Some("boom"));

        let unknown = project_subrun_status_to_spawn("mystery", Some("opaque".to_string()));
        assert_eq!(unknown.status, SPAWN_STATUS_FAILED);
        assert_eq!(unknown.finish_reason, "unknown");
        assert_eq!(unknown.error.as_deref(), Some("opaque"));
    }

    #[tokio::test]
    async fn test_background_completion_unregisters_mailbox() {
        let router = mock_router();
        let spawner = DynamicAgentSpawner::new(router.clone())
            .with_executor(Arc::new(ImmediateSuccessExecutor));
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "main".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            inherited_permissions: None,
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
            live_event_sink: None,
            trace_context: None,
            spawn_tool_call_id: None,
        };
        let input = SpawnAgentInput {
            description: "Background agent".to_string(),
            prompt: "Finish immediately".to_string(),
            agent_type: "explore".to_string(),
            name: Some("bg".to_string()),
            run_in_background: true,
            ..Default::default()
        };

        // Background spawn returns Launched immediately; the completion
        // path unregisters the mailbox asynchronously.
        let _agent_id = match spawner.spawn(input, &context).await.unwrap() {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched, got {other:?}"),
        };

        // Mailbox should be unregistered after completion.
        for _ in 0..20 {
            if router.list_registered_agents().await.is_empty() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        assert!(
            router.list_registered_agents().await.is_empty(),
            "background completion should unregister mailbox"
        );
    }

    #[tokio::test]
    async fn test_spawn_threads_child_recursion_depth_to_run_config() {
        let executor = Arc::new(CapturingDepthExecutor::new());
        let spawner = DynamicAgentSpawner::new(mock_router()).with_executor(executor.clone());
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "main".to_string(),
            recursion_depth: 2,
            parent_is_fork_child: false,
            inherited_permissions: None,
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
            live_event_sink: None,
            trace_context: None,
            spawn_tool_call_id: None,
        };
        let input = SpawnAgentInput {
            description: "Depth test".to_string(),
            prompt: "Run depth test".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: false, // drive the synchronous Completed path
            ..Default::default()
        };

        let result = spawner.spawn(input, &context).await.unwrap();
        assert!(matches!(result, SpawnAgentOutput::Completed { .. }));
        assert_eq!(*executor.captured_depth.lock().unwrap(), Some(3));
    }

    #[tokio::test]
    async fn test_sync_spawn_returns_interrupted_output_for_interrupted_run() {
        let spawner = DynamicAgentSpawner::new(mock_router()).with_executor(Arc::new(
            ImmediateStatusExecutor {
                status: "interrupted",
                finish_reason: "budget_exhausted",
                output: Some("partial"),
                error: None,
            },
        ));
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "main".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            inherited_permissions: None,
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
            live_event_sink: None,
            trace_context: None,
            spawn_tool_call_id: None,
        };
        let input = SpawnAgentInput {
            description: "Sync interrupted agent".to_string(),
            prompt: "Stop before normal completion".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: false,
            ..Default::default()
        };

        let result = spawner.spawn(input, &context).await.unwrap();
        assert!(matches!(
            result,
            SpawnAgentOutput::Interrupted {
                result,
                finish_reason,
                ..
            } if result == "partial" && finish_reason == "budget_exhausted"
        ));
    }

    #[tokio::test]
    async fn test_spawn_rejects_when_recursion_depth_limit_reached() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "main".to_string(),
            recursion_depth: astra_turn_core::agentic_recursion_guard::MAX_AGENT_RECURSION_DEPTH,
            parent_is_fork_child: false,
            inherited_permissions: None,
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
            live_event_sink: None,
            trace_context: None,
            spawn_tool_call_id: None,
        };
        let input = SpawnAgentInput {
            description: "Depth reject".to_string(),
            prompt: "Should fail".to_string(),
            agent_type: "explore".to_string(),
            ..Default::default()
        };

        let result = spawner.spawn(input, &context).await;
        assert!(matches!(result, Err(SpawnError::DepthLimitExceeded(_))));
    }

    #[tokio::test]
    async fn test_sync_spawn_returns_failed_output_for_failed_run() {
        let spawner = DynamicAgentSpawner::new(mock_router()).with_executor(Arc::new(
            ImmediateStatusExecutor {
                status: "failed",
                finish_reason: "failed",
                output: None,
                error: Some("boom"),
            },
        ));
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "main".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            inherited_permissions: None,
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
            live_event_sink: None,
            trace_context: None,
            spawn_tool_call_id: None,
        };
        let input = SpawnAgentInput {
            description: "Sync agent".to_string(),
            prompt: "Fail immediately".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: false, // drive the synchronous Failed path
            ..Default::default()
        };

        let result = spawner.spawn(input, &context).await.unwrap();
        assert!(matches!(
            result,
            SpawnAgentOutput::Failed { ref error, .. } if error == "boom"
        ));
    }

    #[tokio::test]
    async fn background_journal_records_interrupted_finish_reason() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_session("sess-123".to_string())
            .with_executor(Arc::new(ImmediateStatusExecutor {
                status: "interrupted",
                finish_reason: "budget_exhausted",
                output: Some("partial"),
                error: None,
            }));

        let launched = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();
        let agent_id = match launched {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched output, got {other:?}"),
        };

        let status = spawner
            .wait_for_agent(&agent_id, Duration::from_secs(1))
            .await
            .expect("background agent should complete");
        assert!(matches!(status, AgentStatus::Completed { .. }));

        let journal_path = tmp.path().join("sess-123.jsonl");
        let journal = std::fs::read_to_string(journal_path).unwrap();
        assert!(journal.contains("\"type\":\"agent_spawned\""), "{journal}");
        assert!(
            journal.contains("\"type\":\"agent_terminated\""),
            "{journal}"
        );
        assert!(journal.contains("\"status\":\"interrupted\""), "{journal}");
        assert!(
            journal.contains("\"finish_reason\":\"budget_exhausted\""),
            "{journal}"
        );
    }

    #[tokio::test]
    async fn sync_spawn_unknown_status_fails_closed() {
        let spawner = DynamicAgentSpawner::new(mock_router()).with_executor(Arc::new(
            ImmediateStatusExecutor {
                status: "mystery",
                finish_reason: "unknown",
                output: Some("partial"),
                error: None,
            },
        ));
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "main".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            inherited_permissions: None,
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
            live_event_sink: None,
            trace_context: None,
            spawn_tool_call_id: None,
        };
        let input = SpawnAgentInput {
            description: "Unknown status".to_string(),
            prompt: "Should fail closed".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: false,
            ..Default::default()
        };

        let result = spawner.spawn(input, &context).await.unwrap();
        assert!(matches!(
            result,
            SpawnAgentOutput::Failed { ref error, .. } if error.contains("unknown status 'mystery'")
        ));
    }

    #[tokio::test]
    async fn test_inherited_skills_passed_to_run_config() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "parent".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: None,
            inherited_skills: vec!["review-changes".to_string(), "analyze-session".to_string()],
            live_event_sink: None,
            trace_context: None,
            spawn_tool_call_id: None,
        };
        let input = SpawnAgentInput {
            description: "Test with skills".to_string(),
            prompt: "Test".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: true,
            ..Default::default()
        };
        // Skills are stored in context and passed through — spawner launches successfully
        let result = spawner.spawn(input, &context).await.unwrap();
        assert!(matches!(result, SpawnAgentOutput::Launched { .. }));
    }

    #[test]
    fn test_spawn_context_empty_skills_default() {
        let context = SpawnContext {
            parent_run_id: "run-1".to_string(),
            parent_agent_id: "agent-1".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: None,
            inherited_skills: Vec::new(),
            live_event_sink: None,
            trace_context: None,
            spawn_tool_call_id: None,
        };
        assert!(context.inherited_skills.is_empty());
    }

    // ─── HIGH #5: Background agent shutdown drain tests ─────────────────────

    struct BlockingExecutorFactory {
        gate_tx: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        gate_rx: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }

    impl BlockingExecutorFactory {
        fn new() -> Arc<Self> {
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            Arc::new(Self {
                gate_tx: std::sync::Mutex::new(Some(tx)),
                gate_rx: std::sync::Mutex::new(Some(rx)),
            })
        }

        fn unblock(&self) {
            if let Some(tx) = self.gate_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
        }
    }

    #[async_trait]
    impl SpawnAgentExecutor for BlockingExecutorFactory {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            let rx = self.gate_rx.lock().unwrap().take();
            if let Some(rx) = rx {
                let _ = rx.await;
            }
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "completed".into(),
                finish_reason: "normal".into(),
                output: Some("done".into()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
                permission_summary: None,
                permission_requests: 0,
                permission_requests_approved: 0,
                tools_blocked: 0,
            })
        }
    }

    struct PanicExecutor;

    #[async_trait]
    impl SpawnAgentExecutor for PanicExecutor {
        async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            panic!("deliberate panic in background executor");
        }
    }

    fn make_bg_context() -> SpawnContext {
        SpawnContext {
            parent_run_id: "root".to_string(),
            parent_agent_id: "root".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: None,
            inherited_skills: vec![],
            live_event_sink: None,
            trace_context: None,
            spawn_tool_call_id: None,
        }
    }

    fn make_bg_input() -> SpawnAgentInput {
        SpawnAgentInput {
            description: "bg test".to_string(),
            prompt: "do it".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: true,
            ..Default::default()
        }
    }

    fn make_sync_input() -> SpawnAgentInput {
        SpawnAgentInput {
            description: "sync test".to_string(),
            prompt: "do it".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: false,
            ..Default::default()
        }
    }

    /// Concurrency cap: when `with_max_concurrent_agents(n)` is set,
    /// attempting to spawn an (n+1)-th agent while n are still active
    /// must produce `SpawnError::ConcurrencyLimitExceeded` and NOT
    /// silently exceed the cap.
    #[tokio::test]
    async fn spawn_rejects_when_concurrency_cap_reached() {
        let factory = BlockingExecutorFactory::new();
        let factory2 = Arc::clone(&factory);
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(factory as Arc<dyn SpawnAgentExecutor>)
            .with_max_concurrent_agents(2);

        // Two background spawns under the cap of 2 must succeed.
        for i in 0..2 {
            let mut input = make_bg_input();
            input.description = format!("bg-{i}");
            let result = spawner.spawn(input, &make_bg_context()).await;
            assert!(
                matches!(result, Ok(SpawnAgentOutput::Launched { .. })),
                "spawn #{i} under the cap must succeed, got {result:?}"
            );
        }

        // Third spawn while the first two are still in flight (executor
        // is blocked) must hit the cap.
        let result = spawner.spawn(make_bg_input(), &make_bg_context()).await;
        match result {
            Err(SpawnError::ConcurrencyLimitExceeded { active, limit }) => {
                assert_eq!(active, 2);
                assert_eq!(limit, 2);
            }
            other => panic!("expected ConcurrencyLimitExceeded, got {other:?}"),
        }

        // Once we let the first two finish + drain, a fresh spawn is
        // again accepted — the cap is a live measurement, not a one-way
        // counter.
        factory2.unblock();
        spawner
            .shutdown_and_wait(std::time::Duration::from_secs(2))
            .await;
        let after_drain = spawner.spawn(make_bg_input(), &make_bg_context()).await;
        assert!(
            after_drain.is_ok(),
            "after drain the cap must accept new spawns again, got {after_drain:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_named_spawns_reserve_capacity_atomically() {
        let factory = BlockingExecutorFactory::new();
        let factory2 = Arc::clone(&factory);
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(factory as Arc<dyn SpawnAgentExecutor>)
            .with_max_concurrent_agents(1);
        let context = make_bg_context();

        let mut first = make_bg_input();
        first.name = Some("named-a".to_string());
        let mut second = make_bg_input();
        second.name = Some("named-b".to_string());

        let (left, right) = tokio::join!(
            spawner.spawn(first, &context),
            spawner.spawn(second, &context)
        );

        let launched = [left.as_ref(), right.as_ref()]
            .into_iter()
            .filter(|result| matches!(result, Ok(SpawnAgentOutput::Launched { .. })))
            .count();
        let rejected = [left.as_ref(), right.as_ref()]
            .into_iter()
            .filter(|result| {
                matches!(
                    result,
                    Err(SpawnError::ConcurrencyLimitExceeded {
                        active: 1,
                        limit: 1
                    })
                )
            })
            .count();
        assert_eq!(launched, 1, "exactly one spawn should reserve capacity");
        assert_eq!(rejected, 1, "the contending spawn must see the cap");

        assert_eq!(
            spawner.active_agents.read().await.len(),
            1,
            "capacity reservation must not allow active_agents to exceed the cap"
        );

        factory2.unblock();
        spawner
            .shutdown_and_wait(std::time::Duration::from_secs(2))
            .await;
    }

    #[tokio::test]
    async fn spawn_unlimited_when_no_cap_configured() {
        // No cap configured (the historical default) → never errors.
        let factory = BlockingExecutorFactory::new();
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(factory as Arc<dyn SpawnAgentExecutor>);

        for i in 0..6 {
            let mut input = make_bg_input();
            input.description = format!("bg-{i}");
            let result = spawner.spawn(input, &make_bg_context()).await;
            assert!(
                result.is_ok(),
                "spawn #{i} must succeed when no cap is configured, got {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn spawned_agent_state_preserves_fanout_slot_identity() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let mut input = make_bg_input();
        input.fanout_group_id = Some("review-1".to_string());
        input.fanout_group_title = Some("review fanout".to_string());
        input.fanout_target_count = Some(3);
        input.fanout_slot_index = Some(1);

        let output = spawner
            .spawn(input, &make_bg_context())
            .await
            .expect("fanout spawn should be accepted");
        let agent_id = match output {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched output, got {other:?}"),
        };

        let state = spawner
            .get_agent_state(&agent_id)
            .await
            .expect("spawned agent should remain active without an executor");
        let slot = state
            .fanout_slot
            .as_ref()
            .expect("state should keep fanout slot identity");
        assert_eq!(slot.group_id, "review-1");
        assert_eq!(slot.target_count, 3);
        assert_eq!(slot.slot_index, 1);

        let listed = spawner.list_agents("root").await;
        let projected = listed
            .iter()
            .find(|info| info.agent_id == agent_id)
            .expect("list projection should include spawned agent");
        assert_eq!(projected.fanout_slot, state.fanout_slot);
    }

    #[tokio::test]
    async fn spawn_rejects_invalid_fanout_slot_before_reserving_state() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let mut input = make_bg_input();
        input.fanout_group_id = Some("review-1".to_string());
        input.fanout_target_count = Some(3);
        input.fanout_slot_index = Some(3);

        let result = spawner.spawn(input, &make_bg_context()).await;
        assert!(
            matches!(result, Err(SpawnError::InvalidInput(ref message)) if message.contains("outside target_count")),
            "invalid fanout slot must be rejected as invalid input: {result:?}"
        );
        assert!(
            spawner.active_agents.read().await.is_empty(),
            "invalid fanout spawn must not reserve active state"
        );
    }

    #[tokio::test]
    async fn fanout_group_tracks_acceptance_and_rejects_duplicate_slot() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let mut input = make_bg_input();
        input.fanout_group_id = Some("review-1".to_string());
        input.fanout_group_title = Some("review fanout".to_string());
        input.fanout_target_count = Some(3);
        input.fanout_slot_index = Some(1);

        let first = spawner
            .spawn(input.clone(), &make_bg_context())
            .await
            .expect("first slot spawn should be accepted");
        assert!(matches!(first, SpawnAgentOutput::Launched { .. }));

        let duplicate = spawner.spawn(input, &make_bg_context()).await;
        assert!(
            matches!(duplicate, Err(SpawnError::InvalidInput(ref message)) if message.contains("already accepted")),
            "duplicate slot must reject instead of adding a fourth child: {duplicate:?}"
        );

        let groups = spawner.list_fanout_groups().await;
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].title, "review fanout");
        let summary = groups[0].summary();
        assert_eq!(summary.target_count, 3);
        assert_eq!(summary.accepted, 1);
        assert_eq!(summary.active, 1);
        assert_eq!(groups[0].slots[1].requested_description, "bg test");
    }

    #[tokio::test]
    async fn fanout_group_records_spawn_rejected_when_capacity_rejects_slot() {
        let factory = BlockingExecutorFactory::new();
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(factory as Arc<dyn SpawnAgentExecutor>)
            .with_max_concurrent_agents(1);
        let context = make_bg_context();

        let mut first = make_bg_input();
        first.fanout_group_id = Some("review-1".to_string());
        first.fanout_target_count = Some(2);
        first.fanout_slot_index = Some(0);
        let _ = spawner
            .spawn(first, &context)
            .await
            .expect("first fanout slot should be accepted");

        let mut second = make_bg_input();
        second.description = "storage review".to_string();
        second.fanout_group_id = Some("review-1".to_string());
        second.fanout_target_count = Some(2);
        second.fanout_slot_index = Some(1);
        let rejected = spawner.spawn(second, &context).await;
        assert!(
            matches!(rejected, Err(SpawnError::ConcurrencyLimitExceeded { .. })),
            "second slot should hit cap: {rejected:?}"
        );

        let groups = spawner.list_fanout_groups().await;
        assert_eq!(groups.len(), 1);
        let summary = groups[0].summary();
        assert_eq!(summary.target_count, 2);
        assert_eq!(summary.accepted, 1);
        assert_eq!(summary.spawn_rejected, 1);
        assert_eq!(
            groups[0].summary_sentence(),
            "2-agent fanout failed to start fully: 1 accepted, 1 spawn rejected."
        );
        assert_eq!(groups[0].slots[1].requested_description, "storage review");
    }

    #[tokio::test]
    async fn fanout_group_records_spawn_rejected_when_agent_type_is_unknown() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let mut input = make_bg_input();
        input.agent_type = "not-a-real-agent-type".to_string();
        input.description = "storage review".to_string();
        input.fanout_group_id = Some("review-1".to_string());
        input.fanout_group_title = Some("review fanout".to_string());
        input.fanout_target_count = Some(3);
        input.fanout_slot_index = Some(2);

        let rejected = spawner.spawn(input, &make_bg_context()).await;
        assert!(
            matches!(rejected, Err(SpawnError::UnknownAgentType(ref agent_type)) if agent_type == "not-a-real-agent-type"),
            "unknown agent type should remain the direct spawn error: {rejected:?}"
        );
        assert!(
            spawner.active_agents.read().await.is_empty(),
            "failed fanout spawn must not reserve active state"
        );

        let groups = spawner.list_fanout_groups().await;
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].title, "review fanout");
        let summary = groups[0].summary();
        assert_eq!(summary.target_count, 3);
        assert_eq!(summary.accepted, 0);
        assert_eq!(summary.spawn_rejected, 1);
        assert_eq!(summary.active, 0);
        assert_eq!(
            groups[0].slots[2].status,
            AgentFanoutSlotStatus::SpawnRejected
        );
        assert_eq!(groups[0].slots[2].requested_description, "storage review");
        assert!(
            groups[0].slots[2]
                .terminal_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("unknown agent type")),
            "slot should name why spawn failed: {:?}",
            groups[0].slots[2].terminal_reason
        );
        assert_eq!(
            groups[0].summary_sentence(),
            "3-agent fanout failed to start fully: 1 spawn rejected."
        );
    }

    #[tokio::test]
    async fn fanout_group_tracks_terminal_cancel_and_result_collection() {
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>);
        let mut input = make_bg_input();
        input.fanout_group_id = Some("review-1".to_string());
        input.fanout_target_count = Some(3);
        input.fanout_slot_index = Some(0);

        let agent_id = match spawner.spawn(input, &make_bg_context()).await.unwrap() {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched output, got {other:?}"),
        };
        let status = spawner
            .wait_for_agent(&agent_id, std::time::Duration::from_secs(2))
            .await
            .expect("fanout child should complete");
        spawner
            .record_agent_result_collected("root", "root", &agent_id, None, &status)
            .await;

        let groups = spawner.list_fanout_groups().await;
        let summary = groups[0].summary();
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.collected, 1);

        let factory = BlockingExecutorFactory::new();
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(factory as Arc<dyn SpawnAgentExecutor>);
        let mut input = make_bg_input();
        input.fanout_group_id = Some("review-2".to_string());
        input.fanout_target_count = Some(2);
        input.fanout_slot_index = Some(1);
        let agent_id = match spawner.spawn(input, &make_bg_context()).await.unwrap() {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched output, got {other:?}"),
        };
        assert!(spawner.cancel_agent(&agent_id, "user-requested").await);
        let groups = spawner.list_fanout_groups().await;
        let summary = groups[0].summary();
        assert_eq!(summary.cancelled_by_user, 1);
        assert_eq!(summary.active, 0);
    }

    // ── Spawn-cancel UX unhappy paths ─────────────────────────────────────

    #[tokio::test]
    async fn cancelling_a_failed_agent_is_idempotent_and_does_not_overwrite_status() {
        // Race: agent fails → user presses x while the failure has
        // already landed (slight latency between row update and key
        // press). Must not flip the terminal status from Failed →
        // Cancelled, and must NOT spawn anything new.
        struct FailingExecutor;
        #[async_trait]
        impl SpawnAgentExecutor for FailingExecutor {
            async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
                Err("kaboom".into())
            }
        }
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(FailingExecutor) as Arc<dyn SpawnAgentExecutor>);
        let result = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();
        let agent_id = match result {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched, got {other:?}"),
        };

        // Drain to terminal Failed state.
        let _ = spawner
            .wait_for_agent(&agent_id, std::time::Duration::from_secs(2))
            .await;

        // Now cancel — must be a no-op (returns false), terminal state
        // stays Failed.
        let cancelled = spawner
            .cancel_agent(&agent_id, "user-requested via Ctrl+G x")
            .await;
        assert!(
            !cancelled,
            "cancel_agent on a finished (failed) agent must report false"
        );

        let archived = spawner.get_agent_state_any(&agent_id).await.unwrap();
        assert!(
            matches!(archived.status, AgentStatus::Failed { .. }),
            "post-cancel archived status must remain Failed: {:?}",
            archived.status
        );
    }

    #[tokio::test]
    async fn cancelling_an_unknown_agent_id_is_a_noop_returns_false() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let result = spawner.cancel_agent("nonexistent@xyz", "test").await;
        assert!(
            !result,
            "cancel_agent on unknown id must NOT raise; just return false"
        );
    }

    #[tokio::test]
    async fn double_cancel_is_idempotent() {
        // A jittery user double-tapping x must not produce a second
        // cancel side effect (no double archive, no duplicate journal
        // event). Returns false on the second call.
        let factory = BlockingExecutorFactory::new();
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(factory as Arc<dyn SpawnAgentExecutor>);
        let result = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();
        let agent_id = match result {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched, got {other:?}"),
        };

        let first = spawner
            .cancel_agent(&agent_id, "user-requested via Ctrl+G x")
            .await;
        let second = spawner
            .cancel_agent(&agent_id, "user-requested via Ctrl+G x")
            .await;
        assert!(first, "first cancel must own the cancellation");
        assert!(!second, "second cancel must report false (idempotent)");

        // Archive must contain exactly ONE Cancelled record.
        let cancelled_count = spawner
            .completed_agents
            .read()
            .await
            .iter()
            .filter(|s| s.agent_id == agent_id)
            .filter(|s| matches!(s.status, AgentStatus::Cancelled { .. }))
            .count();
        assert_eq!(
            cancelled_count, 1,
            "double-cancel must NOT duplicate the archived Cancelled record"
        );
    }

    #[tokio::test]
    async fn cancel_agent_fills_user_cancelled_flag_for_llm_visibility() {
        // The wire JSON carries `cancelled_by_user: true` so the LLM
        // sees an explicit "do NOT respawn" instruction. This test
        // exercises the spawner side; agent_result_status_tests covers
        // the wire serialization.
        let factory = BlockingExecutorFactory::new();
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(factory as Arc<dyn SpawnAgentExecutor>);
        let result = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();
        let agent_id = match result {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched, got {other:?}"),
        };
        let _ = spawner
            .cancel_agent(&agent_id, "user-requested via Ctrl+G x")
            .await;
        let archived = spawner
            .get_agent_state_any(&agent_id)
            .await
            .expect("cancelled agent should remain queryable");
        match &archived.status {
            AgentStatus::Cancelled { by_user, reason } => {
                assert!(
                    *by_user,
                    "cancel_agent → wire output must report by_user=true so the LLM stops respawning"
                );
                assert!(
                    reason.contains("user-requested"),
                    "reason must surface the human-readable origin: {reason}"
                );
            }
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cap_releases_after_explicit_cancel_so_user_can_spawn_anew() {
        // Concrete user flow: cap=2, two agents in flight, user kills
        // one via x, expects to be able to spawn a third immediately.
        // Without the cap re-check, the user would still be capped at
        // "2 in flight" because cancel_agent didn't free a slot.
        let factory = BlockingExecutorFactory::new();
        let factory2 = Arc::clone(&factory);
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(factory as Arc<dyn SpawnAgentExecutor>)
            .with_max_concurrent_agents(2);

        let mut ids = Vec::new();
        for i in 0..2 {
            let mut input = make_bg_input();
            input.description = format!("bg-{i}");
            let result = spawner.spawn(input, &make_bg_context()).await.unwrap();
            if let SpawnAgentOutput::Launched { agent_id, .. } = result {
                ids.push(agent_id);
            }
        }

        // Hit the cap.
        let third = spawner.spawn(make_bg_input(), &make_bg_context()).await;
        assert!(matches!(
            third,
            Err(SpawnError::ConcurrencyLimitExceeded { .. })
        ));

        // Kill one — slot should free.
        let cancelled = spawner
            .cancel_agent(&ids[0], "user-requested via Ctrl+G x")
            .await;
        assert!(cancelled, "cancel must succeed on a still-running agent");

        // Now a new spawn should be accepted.
        let after_kill = spawner.spawn(make_bg_input(), &make_bg_context()).await;
        assert!(
            after_kill.is_ok(),
            "after explicit cancel the cap must accept a new spawn, got {after_kill:?}"
        );

        factory2.unblock();
        spawner
            .shutdown_and_wait(std::time::Duration::from_secs(2))
            .await;
    }

    /// Sync-mode parity regression: a synchronous spawn must drive the same
    /// finalization path as a background spawn. Concretely the agent must:
    /// (a) leave `active_agents` (no slow leak), and
    /// (b) appear in `completed_agents` (so wait_for / get_agent_history /
    ///     archive-based observers see the terminal state).
    ///
    /// History: sync-mode used to call only `update_status` +
    /// `unregister_mailbox`, leaving the agent in `active_agents` forever and
    /// never archiving. Two paths produced different persisted state for the
    /// same logical event.
    #[tokio::test]
    async fn sync_spawn_archives_into_completed_agents_and_clears_active() {
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>);

        let result = spawner
            .spawn(make_sync_input(), &make_bg_context())
            .await
            .unwrap();
        assert!(
            matches!(result, SpawnAgentOutput::Completed { .. }),
            "sync spawn must return Completed for a fast-success child, got {result:?}"
        );

        // active_agents must be empty (sync child has finished).
        let active = spawner.active_agents.read().await;
        assert!(
            active.is_empty(),
            "sync-mode finalize must remove from active_agents; got {} entries",
            active.len()
        );
        drop(active);

        // completed_agents must contain the run.
        let completed = spawner.completed_agents.read().await;
        assert_eq!(
            completed.len(),
            1,
            "sync-mode finalize must archive into completed_agents"
        );
        assert!(matches!(completed[0].status, AgentStatus::Completed { .. }));
    }

    #[tokio::test]
    async fn foreground_sync_agent_can_be_promoted_to_background_while_waiting() {
        let factory = BlockingExecutorFactory::new();
        let spawner = Arc::new(
            DynamicAgentSpawner::new(mock_router())
                .with_executor(factory.clone() as Arc<dyn SpawnAgentExecutor>),
        );

        let spawn_task = {
            let spawner = Arc::clone(&spawner);
            tokio::spawn(async move { spawner.spawn(make_sync_input(), &make_bg_context()).await })
        };

        for _ in 0..50 {
            if !spawner.list_all_agents().await.is_empty() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        assert!(
            spawner
                .promote_foreground_agent_to_background(Some("not-this-parent"))
                .await
                .is_none(),
            "promotion must respect the parent run filter"
        );

        let promoted = spawner
            .promote_foreground_agent_to_background(Some("root"))
            .await
            .expect("foreground sync agent should be promotable");
        let spawn_result = tokio::time::timeout(Duration::from_secs(1), spawn_task)
            .await
            .expect("promotion must wake the waiting spawn call")
            .expect("spawn task must not panic")
            .expect("spawn must succeed after promotion");
        assert!(
            matches!(
                spawn_result,
                SpawnAgentOutput::Launched { ref agent_id, .. } if agent_id == &promoted.agent_id
            ),
            "promoted sync spawn must return Launched with the same runtime id, got {spawn_result:?}"
        );

        assert!(
            spawner
                .promote_foreground_agent_to_background(Some("root"))
                .await
                .is_none(),
            "already-promoted agents must not be promoted twice"
        );

        let still_waiting = spawner
            .wait_for_agent(&promoted.agent_id, Duration::from_millis(20))
            .await;
        assert!(
            still_waiting.is_none(),
            "promotion must not complete or cancel the child agent"
        );

        factory.unblock();
        let terminal = spawner
            .wait_for_agent(&promoted.agent_id, Duration::from_secs(1))
            .await
            .expect("promoted background agent should still finish normally");
        assert!(matches!(terminal, AgentStatus::Completed { .. }));
        let archived = spawner
            .get_agent_state_any(&promoted.agent_id)
            .await
            .expect("promoted agent should remain in history");
        assert!(
            archived.run_in_background,
            "promoted agent history must record background mode"
        );

        spawner
            .shutdown_and_wait(std::time::Duration::from_secs(1))
            .await;
    }

    #[tokio::test]
    async fn sync_spawn_failure_archives_with_failed_status() {
        struct FailingExecutor;
        #[async_trait]
        impl SpawnAgentExecutor for FailingExecutor {
            async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
                Err("kaboom".to_string())
            }
        }
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(FailingExecutor) as Arc<dyn SpawnAgentExecutor>);

        let _ = spawner
            .spawn(make_sync_input(), &make_bg_context())
            .await
            .unwrap();

        assert!(
            spawner.active_agents.read().await.is_empty(),
            "active_agents must be empty after sync-mode failure finalize"
        );
        let completed = spawner.completed_agents.read().await;
        assert_eq!(completed.len(), 1, "failed sync agent must be archived");
        assert!(
            matches!(completed[0].status, AgentStatus::Failed { .. }),
            "archived status must reflect failure: got {:?}",
            completed[0].status
        );
    }

    /// Background spawn returns immediately even when the child is fast.
    /// Claude Code parity: background means "launch now, report later", so
    /// the parent can fan out N agents without being serialized by child work.
    #[tokio::test]
    async fn background_spawn_returns_launched_for_fast_child() {
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>);

        let result = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();

        assert!(
            matches!(result, SpawnAgentOutput::Launched { .. }),
            "background spawn must return Launched immediately, got {result:?}"
        );
    }

    /// HIGH #5: background agent tracked in JoinSet; shutdown_and_wait drains it.
    #[tokio::test]
    async fn background_agent_tracked_and_drained_on_shutdown() {
        let factory = BlockingExecutorFactory::new();
        let factory2 = Arc::clone(&factory);
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(factory as Arc<dyn SpawnAgentExecutor>);

        let result = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();
        assert!(
            matches!(result, SpawnAgentOutput::Launched { .. }),
            "background spawn should return Launched"
        );

        // Task is in flight — JoinSet should have at least one entry.
        assert!(
            spawner.background_task_count() > 0,
            "background task must be tracked before unblocking"
        );

        // Unblock the executor so it can complete.
        factory2.unblock();

        // shutdown_and_wait must drain the JoinSet within the deadline.
        spawner
            .shutdown_and_wait(std::time::Duration::from_secs(2))
            .await;

        assert_eq!(
            spawner.background_task_count(),
            0,
            "all background tasks must be drained after shutdown"
        );
    }

    #[tokio::test]
    async fn background_spawn_reports_running_status_immediately() {
        let factory = BlockingExecutorFactory::new();
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(factory as Arc<dyn SpawnAgentExecutor>);

        let result = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();
        let agent_id = match result {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched, got {other:?}"),
        };

        let state = spawner
            .get_agent_state_any(&agent_id)
            .await
            .expect("launched agent should be visible");
        assert!(
            matches!(state.status, AgentStatus::Running { .. }),
            "background agent should not appear stuck in Initializing: {:?}",
            state.status
        );
    }

    /// Background agent results are returned by shutdown_and_wait.
    #[tokio::test]
    async fn shutdown_returns_completed_background_results() {
        let factory = BlockingExecutorFactory::new();
        let factory2 = Arc::clone(&factory);
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(factory as Arc<dyn SpawnAgentExecutor>);

        let result = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();
        assert!(matches!(result, SpawnAgentOutput::Launched { .. }));

        factory2.unblock();

        let bg_results = spawner
            .shutdown_and_wait(std::time::Duration::from_secs(2))
            .await;

        // The mock executor returns a completed result, so we should
        // see it in the drain output.
        assert!(
            !bg_results.is_empty(),
            "shutdown_and_wait must return completed background agent results"
        );
    }

    /// wait_for_agent returns immediately when child is already completed.
    #[tokio::test]
    async fn wait_for_agent_returns_immediately_when_completed() {
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>);

        let result = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();
        let agent_id = match result {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched, got {other:?}"),
        };

        // Wait for background task to complete via the notifier.
        let status = spawner
            .wait_for_agent(&agent_id, std::time::Duration::from_secs(5))
            .await;
        assert!(
            matches!(status, Some(AgentStatus::Completed { .. })),
            "wait_for_agent must return Completed, got {status:?}"
        );
    }

    #[tokio::test]
    async fn wait_for_agent_wakes_multiple_concurrent_waiters() {
        let factory = BlockingExecutorFactory::new();
        let factory2 = Arc::clone(&factory);
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(factory as Arc<dyn SpawnAgentExecutor>);

        let result = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();
        let agent_id = match result {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched, got {other:?}"),
        };

        let waiter_a = spawner.wait_for_agent(&agent_id, std::time::Duration::from_secs(2));
        let waiter_b = spawner.wait_for_agent(&agent_id, std::time::Duration::from_secs(2));
        tokio::task::yield_now().await;
        factory2.unblock();

        let (a, b) = tokio::join!(waiter_a, waiter_b);
        assert!(matches!(a, Some(AgentStatus::Completed { .. })), "a={a:?}");
        assert!(matches!(b, Some(AgentStatus::Completed { .. })), "b={b:?}");
    }

    #[tokio::test]
    async fn cancel_agent_marks_only_target_as_cancelled() {
        let factory = BlockingExecutorFactory::new();
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(factory as Arc<dyn SpawnAgentExecutor>);

        let result = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();
        let agent_id = match result {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched, got {other:?}"),
        };

        assert!(
            spawner
                .cancel_agent(&agent_id, "turn budget exhausted")
                .await,
            "cancel_agent should report success for running background child"
        );

        let status = spawner
            .wait_for_agent(&agent_id, std::time::Duration::from_secs(1))
            .await;
        assert!(
            matches!(status, Some(AgentStatus::Cancelled { .. })),
            "status={status:?}"
        );

        let archived = spawner
            .get_agent_state_any(&agent_id)
            .await
            .expect("cancelled agent should remain queryable");
        assert!(
            matches!(archived.status, AgentStatus::Cancelled { .. }),
            "archived status must be cancelled: {:?}",
            archived.status
        );
        // `cancel_agent` is the user-driven path → must surface the
        // user-driven flag so the wire output instructs the LLM not
        // to respawn.
        assert!(
            matches!(
                &archived.status,
                AgentStatus::Cancelled { by_user: true, .. }
            ),
            "cancel_agent must mark the cancellation as user-driven: {:?}",
            archived.status
        );

        let bg_results = spawner
            .shutdown_and_wait(std::time::Duration::from_secs(1))
            .await;
        assert!(
            bg_results.iter().all(|(id, _)| id != &agent_id),
            "cancelled agent must not surface as completed result: {bg_results:?}"
        );
    }

    /// REGRESSION (reviewer L2-3): after `spawn(run_in_background:true)`
    /// returned `Launched` immediately (the auto-wait timeout was
    /// removed in commit a4719d7ca), there's a tiny window where
    /// the child future has been pushed to `JoinSet` but hasn't yet
    /// polled to completion — or even started. A parent that
    /// immediately calls `get_result(agent_id)` MUST still resolve
    /// correctly: either block until the child completes, or return
    /// the result if it already did.
    ///
    /// Pin both halves of that race:
    ///   1. spawn → wait_for_agent(0ms timeout) sees no completion yet
    ///      (the child hasn't started executing when we ask
    ///      immediately after spawn returns)
    ///   2. spawn → wait_for_agent(generous timeout) eventually
    ///      resolves to Completed (the notifier hooks fire after the
    ///      child finishes)
    #[tokio::test]
    async fn spawn_then_immediate_get_result_resolves_correctly() {
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>);

        let result = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();
        let agent_id = match result {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched immediately, got {other:?}"),
        };

        // Generous wait — the child WILL complete because
        // ImmediateSuccessExecutor returns synchronously, but we must
        // not race past the notifier registration or assume the
        // child has already finished by the time we call.
        let status = spawner
            .wait_for_agent(&agent_id, std::time::Duration::from_secs(5))
            .await;
        assert!(
            matches!(status, Some(AgentStatus::Completed { .. })),
            "spawn → immediate get_result must resolve to Completed; got {status:?}"
        );
    }

    /// wait_for_agent times out when agent doesn't exist.
    #[tokio::test]
    async fn wait_for_agent_times_out_for_unknown_agent() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let status = spawner
            .wait_for_agent("nonexistent", std::time::Duration::from_millis(100))
            .await;
        assert!(status.is_none(), "unknown agent should return None");
    }

    #[tokio::test]
    async fn wait_for_agent_outcome_distinguishes_unknown_agent() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let outcome = spawner
            .wait_for_agent_outcome("nonexistent", std::time::Duration::from_millis(100))
            .await;
        assert_eq!(outcome, WaitForAgentOutcome::Unknown);
    }

    /// wait_for_agent_outcome returns TimedOut when the agent is known
    /// (registered in the tracker) but doesn't complete before the timeout.
    #[tokio::test]
    async fn wait_for_agent_outcome_distinguishes_timed_out_agent() {
        let factory = BlockingExecutorFactory::new();
        let factory2 = Arc::clone(&factory);
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(factory as Arc<dyn SpawnAgentExecutor>);

        let result = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();
        let agent_id = match result {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched, got {other:?}"),
        };

        // The executor blocks until explicitly unblocked, so a short
        // timeout should yield TimedOut — the agent is known (registered
        // in the tracker) but hasn't completed.
        let outcome = spawner
            .wait_for_agent_outcome(&agent_id, std::time::Duration::from_millis(100))
            .await;
        assert_eq!(outcome, WaitForAgentOutcome::TimedOut);

        // Clean up: unblock and drain.
        factory2.unblock();
        spawner
            .shutdown_and_wait(std::time::Duration::from_secs(2))
            .await;
    }

    /// Late-waiter coverage: even when the notifier is already gone (finalize
    /// has run to completion AND removed the entry), the pre-check at the
    /// top of `wait_for_agent_outcome` reads `completed_agents` and surfaces
    /// the terminal status. Asserting this explicitly so a refactor that
    /// removes the pre-check or reorders archive vs notifier-removal would
    /// trip immediately.
    #[tokio::test]
    async fn wait_for_agent_outcome_late_arrival_returns_completed_status() {
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>);

        let result = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();
        let agent_id = match result {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched, got {other:?}"),
        };

        // Drive the agent fully through finalize: archived AND notifier
        // removed. Both conditions must be observable before we test the
        // late-waiter path.
        for _ in 0..1000 {
            let archived = spawner
                .completed_agents
                .read()
                .await
                .iter()
                .any(|s| s.agent_id == agent_id);
            let notifier_gone = !spawner
                .completion_notifiers
                .read()
                .await
                .contains_key(&agent_id);
            if archived && notifier_gone {
                break;
            }
            tokio::task::yield_now().await;
        }
        // Sanity: precondition for the test.
        assert!(
            !spawner
                .completion_notifiers
                .read()
                .await
                .contains_key(&agent_id),
            "test precondition failed: notifier should already be gone"
        );

        // Late wait: notifier missing, so the lookup at line 1471 returns
        // None. The bug was: classify_wait_failure then reports TimedOut
        // (because get_agent_state_any sees the archived state). Correct
        // behaviour: surface the terminal Status from completed_agents.
        let outcome = spawner
            .wait_for_agent_outcome(&agent_id, std::time::Duration::from_millis(50))
            .await;
        assert!(
            matches!(
                outcome,
                WaitForAgentOutcome::Status(AgentStatus::Completed { .. })
            ),
            "late waiter must see Completed terminal status, got {outcome:?}"
        );
    }

    /// Regression: background task completes BEFORE shutdown_and_wait is called.
    /// Uses BlockingExecutorFactory so the child finishes before
    /// shutdown_and_wait is called, but after spawn returned Launched.
    #[tokio::test]
    async fn shutdown_returns_results_even_when_task_completed_before_drain() {
        let factory = BlockingExecutorFactory::new();
        let factory2 = Arc::clone(&factory);
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(factory as Arc<dyn SpawnAgentExecutor>);

        let result = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();
        assert!(matches!(result, SpawnAgentOutput::Launched { .. }));

        // Unblock the child, then let it finish before we drain.
        factory2.unblock();
        for _ in 0..200 {
            if !spawner.completed_agents.read().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }

        let bg_results = spawner
            .shutdown_and_wait(std::time::Duration::from_secs(2))
            .await;

        assert!(
            !bg_results.is_empty(),
            "shutdown_and_wait must return results even when background \
             task completed before drain was called"
        );
    }

    /// HIGH #5: background agent that panics does not leave a zombie in the JoinSet.
    #[tokio::test]
    async fn background_agent_panic_does_not_leave_zombie() {
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(PanicExecutor) as Arc<dyn SpawnAgentExecutor>);

        let _ = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();

        // Give the panic time to propagate; shutdown_and_wait catches the JoinError.
        spawner
            .shutdown_and_wait(std::time::Duration::from_secs(2))
            .await;

        assert_eq!(
            spawner.background_task_count(),
            0,
            "panicked background task must not leave zombie in JoinSet"
        );
    }

    // ─── HIGH #1: completion_notifiers cleaned up after notify ────────────

    /// After a background agent completes and wait_for_agent returns,
    /// the completion_notifiers map must be empty (entry removed).
    #[tokio::test]
    async fn completion_notifiers_cleaned_after_wait() {
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>);

        let result = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();
        let agent_id = match result {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched, got {other:?}"),
        };

        // Wait for agent to ensure it has fully completed.
        let _ = spawner
            .wait_for_agent(&agent_id, std::time::Duration::from_secs(5))
            .await;

        // Give handle_completion a moment to finish (it runs in the
        // background task which may still be executing notify_completion).
        for _ in 0..100 {
            if spawner.completion_notifiers.read().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }

        assert!(
            spawner.completion_notifiers.read().await.is_empty(),
            "completion_notifiers must be empty after agent completes"
        );
    }

    /// shutdown_and_wait also cleans up any leftover completion notifiers.
    #[tokio::test]
    async fn shutdown_cleans_completion_notifiers() {
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>);

        let _ = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();

        spawner
            .shutdown_and_wait(std::time::Duration::from_secs(2))
            .await;

        assert!(
            spawner.completion_notifiers.read().await.is_empty(),
            "shutdown_and_wait must clear completion_notifiers"
        );
    }

    // ─── HIGH #2: active_agents cleaned after archive ──────────────────

    /// After a background agent completes and is archived,
    /// active_agents must not contain the agent any more.
    #[tokio::test]
    async fn active_agents_empty_after_background_completion() {
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>);

        let result = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();
        let agent_id = match result {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched, got {other:?}"),
        };

        // Wait for agent to fully complete and archive.
        let _ = spawner
            .wait_for_agent(&agent_id, std::time::Duration::from_secs(5))
            .await;

        // Give handle_completion/archive_agent time to finish.
        for _ in 0..100 {
            if spawner.active_agents.read().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }

        assert!(
            spawner.active_agents.read().await.is_empty(),
            "active_agents must be empty after agent completes and is archived"
        );
        assert!(
            spawner
                .background_agent_ids
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty(),
            "background_agent_ids must not retain finalized agents"
        );

        // Agent should be in completed_agents instead.
        assert!(
            !spawner.completed_agents.read().await.is_empty(),
            "completed_agents must contain the archived agent"
        );
    }

    // ---------------------------------------------------------------
    // PR 4.5 — prefix-inheritance wiring into the live spawner.
    //
    // These tests define the expected behavior:
    // 1. No prefix_store configured → spawn unaffected (backwards
    //    compatible; every existing test continues passing).
    // 2. prefix_store + feature on + matching captured prefix →
    //    spawn succeeds; resolve outcome queryable as `Resolved`.
    // 3. prefix_store + feature on + missing prefix + required=true
    //    → spawn returns SpawnError::PrefixInheritanceRequired.
    // 4. prefix_store + feature on + missing prefix + required=false
    //    → spawn succeeds; resolve outcome is `Fallback`.
    // ---------------------------------------------------------------

    use astra_turn_core::fork_capture::{CaptureRequest, capture_parent_prefix};
    use astra_turn_core::fork_prefix::{
        CacheMode, ProviderKind, SystemBlock, ThinkingConfigSlice, ToolSchemaEntry,
        hash_tool_schema,
    };
    use astra_turn_core::fork_prefix_store::{InMemoryPrefixStore, PrefixCaptureSink};
    use astra_turn_core::fork_resolve::PrefixResolveOutcome;
    use astra_turn_core::orchestration_spawn_tool::InheritPrefixSpec;

    fn wall_now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn capture_parent_for(store: &dyn PrefixCaptureSink, parent_run_id: &str, model: &str) {
        let schema = serde_json::json!({"function": {"name": "bash"}});
        let (bytes, hash) = hash_tool_schema(&schema);
        let req = CaptureRequest {
            parent_run_id: parent_run_id.to_string(),
            parent_turn_seq: 1,
            provider: ProviderKind::Anthropic,
            model_id: model.to_string(),
            thinking: Some(ThinkingConfigSlice {
                enabled: false,
                budget_tokens: 0,
                kind: "disabled".into(),
            }),
            system_blocks: vec![SystemBlock {
                bytes: b"sys".to_vec(),
                has_cache_control: true,
            }],
            tool_schemas: vec![ToolSchemaEntry {
                name: "bash".into(),
                canonical_bytes: bytes,
                hash,
            }],
            beta_headers: vec![],
            // Must be a JSON array of message objects — the
            // reconstructor (PR 5b) parses this back on the child
            // spawn path. A raw non-JSON marker would make
            // `reconstruct_messages` fail and produce a None
            // inherited_prefix, hiding the actual resolve behavior
            // we're testing.
            canonical_prefix_bytes: serde_json::to_vec(&serde_json::json!([
                {"role": "user", "content": "parent message"}
            ]))
            .expect("static json encodes"),
            cache_mode: CacheMode::Write,
            captured_at_secs: wall_now_secs(),
            microcompact_fired_in_turn: false,
        };
        let _ = capture_parent_prefix(req, store);
    }

    fn parent_context(run_id: &str) -> SpawnContext {
        SpawnContext {
            parent_run_id: run_id.to_string(),
            parent_agent_id: "parent".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: None,
            inherited_skills: vec![],
            live_event_sink: None,
            trace_context: None,
            spawn_tool_call_id: None,
        }
    }

    fn child_with_inherit(required: bool) -> SpawnAgentInput {
        SpawnAgentInput {
            description: "child".into(),
            prompt: "work".into(),
            agent_type: "explore".into(),
            // Match the captured parent's model/thinking. Keep this
            // explicit so the test stays stable even though built-in
            // agent types now inherit the server default model.
            model: Some(TEST_CHILD_MODEL.into()),
            inherit_prefix: Some(InheritPrefixSpec {
                from_run_id: None, // use caller's run id
                required,
            }),
            ..Default::default()
        }
    }

    const TEST_CHILD_MODEL: &str = "claude-test-model";

    #[tokio::test]
    async fn spawn_without_prefix_store_is_backwards_compatible() {
        // Existing callers that never configured a prefix_store must
        // continue to work identically — this test pins the
        // additive-only property. Even with inherit_prefix set in
        // the input, spawn must succeed (prefix request silently
        // has no effect without a store).
        let spawner = DynamicAgentSpawner::new(mock_router());
        let input = child_with_inherit(false);
        let ctx = parent_context("parent-unused");
        let result = spawner.spawn(input, &ctx).await;
        assert!(
            matches!(result, Ok(SpawnAgentOutput::Launched { .. })),
            "spawn must succeed without store even when inherit_prefix is set, got {result:?}"
        );
    }

    #[tokio::test]
    async fn spawn_resolves_matching_captured_prefix() {
        let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
        let spawner = DynamicAgentSpawner::new(mock_router()).with_prefix_store(store.clone());
        capture_parent_for(&*store, "run-parent-A", TEST_CHILD_MODEL);

        let input = child_with_inherit(false);
        let ctx = parent_context("run-parent-A");
        let out = spawner.spawn(input, &ctx).await.unwrap();
        let agent_id = match out {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched, got {other:?}"),
        };
        let outcome = spawner
            .last_prefix_resolve(&agent_id)
            .await
            .expect("outcome must be recorded for a spawn that requested inheritance");
        assert!(
            matches!(outcome, PrefixResolveOutcome::Resolved { .. }),
            "expected Resolved, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn spawn_with_required_and_missing_prefix_hard_fails() {
        let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
        let spawner = DynamicAgentSpawner::new(mock_router()).with_prefix_store(store);
        // No capture — store is empty.
        let input = child_with_inherit(true); // required
        let ctx = parent_context("run-no-capture");
        let result = spawner.spawn(input, &ctx).await;
        match result {
            Err(SpawnError::PrefixInheritanceRequired { .. }) => {}
            other => panic!("expected PrefixInheritanceRequired, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn spawn_with_optional_and_missing_prefix_falls_back() {
        let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
        let spawner = DynamicAgentSpawner::new(mock_router()).with_prefix_store(store);
        let input = child_with_inherit(false); // not required
        let ctx = parent_context("run-no-capture");
        let out = spawner.spawn(input, &ctx).await.unwrap();
        let agent_id = match out {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched, got {other:?}"),
        };
        let outcome = spawner.last_prefix_resolve(&agent_id).await.unwrap();
        assert!(
            matches!(outcome, PrefixResolveOutcome::Fallback { .. }),
            "expected Fallback, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn spawn_without_inherit_spec_records_disabled() {
        // inherit_prefix=None → outcome should be Disabled, regardless
        // of whether a store is configured or the flag is on.
        let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
        let spawner = DynamicAgentSpawner::new(mock_router()).with_prefix_store(store);
        let input = SpawnAgentInput {
            description: "child".into(),
            prompt: "work".into(),
            agent_type: "explore".into(),
            run_in_background: true,
            ..Default::default()
        };
        let ctx = parent_context("run-parent");
        let out = spawner.spawn(input, &ctx).await.unwrap();
        let agent_id = match out {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched, got {other:?}"),
        };
        let outcome = spawner.last_prefix_resolve(&agent_id).await.unwrap();
        assert!(
            matches!(outcome, PrefixResolveOutcome::Disabled),
            "expected Disabled, got {outcome:?}"
        );
    }

    // ---------------------------------------------------------------
    // PR 5.5 — inherited_prefix populates SpawnRunConfig when resolver
    // returns Resolved, and is None in every other outcome path. The
    // executor is the only place that sees SpawnRunConfig; we use a
    // CapturingPrefixExecutor to introspect what the spawner built.
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn resolved_prefix_populates_spawn_run_config_inherited_prefix() {
        let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
        let exec = Arc::new(CapturingPrefixExecutor::new());
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_prefix_store(store.clone())
            .with_executor(exec.clone() as Arc<dyn SpawnAgentExecutor>);

        capture_parent_for(&*store, "run-parent-A", TEST_CHILD_MODEL);

        // Sync spawn (background=false) so the executor runs before
        // spawn() returns and we can read the captured config.
        let mut input = child_with_inherit(false);
        input.run_in_background = false;
        let ctx = parent_context("run-parent-A");
        let _ = spawner.spawn(input, &ctx).await.unwrap();

        let captured = exec
            .take_captured()
            .expect("executor must have been called exactly once");
        let inherited = captured.expect("Resolved outcome must produce Some inherited_prefix");
        assert_eq!(inherited.parent_run_id, "run-parent-A");
        assert!(!inherited.prefix_id.is_empty());
        assert!(
            !inherited.prefix_messages.is_empty(),
            "prefix_messages must carry the parent's captured messages"
        );
        assert!(matches!(
            inherited.provider,
            astra_turn_core::fork_prefix::ProviderKind::Anthropic
        ));
    }

    #[tokio::test]
    async fn fallback_outcome_leaves_inherited_prefix_none() {
        // Resolver Fallback (no matching parent capture) must yield
        // `inherited_prefix: None` on the config — executor can tell
        // from the config alone that it should run fresh.
        let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
        let exec = Arc::new(CapturingPrefixExecutor::new());
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_prefix_store(store)
            .with_executor(exec.clone() as Arc<dyn SpawnAgentExecutor>);

        let mut input = child_with_inherit(false);
        input.run_in_background = false;
        let ctx = parent_context("run-no-capture");
        let _ = spawner.spawn(input, &ctx).await.unwrap();

        let captured = exec.take_captured().unwrap();
        assert!(
            captured.is_none(),
            "Fallback outcome must produce None inherited_prefix, got Some(...)"
        );
    }

    #[tokio::test]
    async fn disabled_outcome_leaves_inherited_prefix_none() {
        // No inherit_prefix spec at all (most common path) — outcome
        // is Disabled and inherited_prefix is None, same as Fallback.
        let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
        let exec = Arc::new(CapturingPrefixExecutor::new());
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_prefix_store(store)
            .with_executor(exec.clone() as Arc<dyn SpawnAgentExecutor>);

        let input = SpawnAgentInput {
            description: "no inherit".into(),
            prompt: "work".into(),
            agent_type: "explore".into(),
            run_in_background: false,
            ..Default::default()
        };
        let ctx = parent_context("run-parent");
        let _ = spawner.spawn(input, &ctx).await.unwrap();

        let captured = exec.take_captured().unwrap();
        assert!(
            captured.is_none(),
            "Disabled outcome must produce None inherited_prefix"
        );
    }

    #[tokio::test]
    async fn inherited_prefix_messages_round_trip_byte_identical() {
        // Cache-reuse precondition: the executor-visible
        // prefix_messages, when re-serialized, must equal the bytes
        // the capture recorded. Without this, no prompt cache hit is
        // possible on the child's first API call.
        let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
        let exec = Arc::new(CapturingPrefixExecutor::new());
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_prefix_store(store.clone())
            .with_executor(exec.clone() as Arc<dyn SpawnAgentExecutor>);
        capture_parent_for(&*store, "run-parent-bid", TEST_CHILD_MODEL);

        // Grab the captured prefix bytes from the sink directly so we
        // can diff against what the executor eventually sees.
        let stored_prefix = store
            .get_prefix("run-parent-bid")
            .expect("capture must have persisted");
        let captured_canonical = stored_prefix.canonical_prefix_bytes().clone();

        let mut input = child_with_inherit(false);
        input.run_in_background = false;
        let _ = spawner
            .spawn(input, &parent_context("run-parent-bid"))
            .await
            .unwrap();

        let captured = exec.take_captured().unwrap().unwrap();
        let reserialized = serde_json::to_vec(&captured.prefix_messages).unwrap();
        assert_eq!(
            reserialized.as_slice(),
            captured_canonical.as_slice(),
            "re-serialized prefix_messages must equal captured canonical bytes"
        );
    }

    #[tokio::test]
    async fn fork_child_cannot_request_nested_prefix_inheritance() {
        let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
        let spawner = DynamicAgentSpawner::new(mock_router()).with_prefix_store(store.clone());
        capture_parent_for(&*store, "run-fork-parent", TEST_CHILD_MODEL);

        let mut ctx = parent_context("run-fork-parent");
        ctx.parent_is_fork_child = true;

        let result = spawner.spawn(child_with_inherit(false), &ctx).await;
        match result {
            Err(SpawnError::NestedForkInheritanceRejected) => {}
            other => panic!("expected nested fork inheritance rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fork_child_can_still_spawn_fresh_child_without_inheritance() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let mut ctx = parent_context("run-fork-parent");
        ctx.parent_is_fork_child = true;
        let input = SpawnAgentInput {
            description: "fresh child".into(),
            prompt: "do unrelated work".into(),
            agent_type: "explore".into(),
            ..Default::default()
        };

        let result = spawner.spawn(input, &ctx).await;
        assert!(
            matches!(result, Ok(SpawnAgentOutput::Launched { .. })),
            "fork children must still be able to spawn ordinary non-inheriting children: {result:?}"
        );
    }

    #[tokio::test]
    async fn inherited_prefix_expected_cache_read_tokens_uses_nonzero_estimate() {
        let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
        let exec = Arc::new(CapturingPrefixExecutor::new());
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_prefix_store(store.clone())
            .with_executor(exec.clone() as Arc<dyn SpawnAgentExecutor>);
        capture_parent_for(&*store, "run-parent-estimate", TEST_CHILD_MODEL);

        let mut input = child_with_inherit(false);
        input.run_in_background = false;
        let _ = spawner
            .spawn(input, &parent_context("run-parent-estimate"))
            .await
            .unwrap();

        let inherited = exec.take_captured().unwrap().unwrap();
        assert!(
            inherited.expected_cache_read_tokens > 0,
            "resolved fork children need a nonzero expected cache-read baseline"
        );
    }

    #[test]
    fn inherited_prefix_expected_cache_read_tokens_matches_canonical_prefix_size() {
        let store = InMemoryPrefixStore::new();
        capture_parent_for(&store, "run-parent-estimate-shape", TEST_CHILD_MODEL);
        let prefix = store
            .get_prefix("run-parent-estimate-shape")
            .expect("capture must have persisted");

        let expected = u64::try_from(prefix.size_bytes().div_ceil(4)).unwrap_or(u64::MAX);
        assert_eq!(
            estimate_cache_read_tokens(&prefix),
            expected,
            "cache-read estimate should be derived from the canonical serialized prefix once"
        );
    }
}
