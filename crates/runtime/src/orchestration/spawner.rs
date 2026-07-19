//! Dynamic agent spawner — runtime agent creation and lifecycle management.

use crate::messaging::SubRunInfo;
use astra_messaging::router::AgentMailboxRouter;
use astra_messaging::types::{AgentAddress, AgentMessage, MessagePayload, MessageTarget};
use astra_turn_core::fork_prefix_store::PrefixCaptureSink;
use astra_turn_core::fork_reconstruct::reconstruct_messages;
use astra_turn_core::fork_resolve::{
    PrefixResolveOutcome, SpawnResolveContext, resolve_inherit_prefix,
};
use astra_turn_core::orchestration_context_cache::SharedContextCache;
use astra_turn_core::orchestration_fanout_group::{
    AgentFanoutGroupProjection, AgentFanoutSlotIdentity, AgentFanoutSlotStatus, AgentFanoutStatus,
};
use astra_turn_core::orchestration_progress::{
    AgentProgressEvent, ProgressBroadcaster, ProgressEventType,
};
use astra_turn_core::orchestration_spawn_tool::{SpawnAgentInput, SpawnAgentOutput};
use astra_turn_core::trace_event::{TraceContext, TraceEvent, TraceEventWriter};
use futures_util::FutureExt;

use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
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

/// Stable causes shared by descendant cancellation and durable run cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DescendantCancellationReason {
    AncestorCancelled,
}

impl DescendantCancellationReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::AncestorCancelled => "ancestor run cancelled before child completion",
        }
    }
}

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
    if agent_completion_is_interrupted(finish_reason) {
        SPAWN_STATUS_INTERRUPTED
    } else {
        SPAWN_STATUS_COMPLETED
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnStatusProjection {
    pub status: &'static str,
    pub finish_reason: String,
    pub error: Option<String>,
}

pub fn project_subrun_status_to_spawn(
    subrun_status: &str,
    mut error: Option<String>,
) -> SpawnStatusProjection {
    let (status, default_finish_reason, interruption_reason_from_error) = match subrun_status {
        astra_core::STATUS_COMPLETED => (SPAWN_STATUS_COMPLETED, "normal", false),
        astra_core::STATUS_WAITING => (SPAWN_STATUS_WAITING, "waiting", false),
        astra_core::STATUS_CANCELLED => (SPAWN_STATUS_CANCELLED, "cancelled", false),
        astra_core::STATUS_FAILED => (SPAWN_STATUS_FAILED, "failed", false),
        astra_core::STATUS_PAUSED => (SPAWN_STATUS_INTERRUPTED, "paused", true),
        astra_services::coordination::AGENT_RESULT_STATUS_PARTIAL => {
            (SPAWN_STATUS_INTERRUPTED, "partial", true)
        }
        _ => (SPAWN_STATUS_FAILED, "unknown", false),
    };

    let finish_reason = if interruption_reason_from_error {
        error
            .take()
            .filter(|reason| !reason.trim().is_empty())
            .unwrap_or_else(|| default_finish_reason.to_string())
    } else {
        default_finish_reason.to_string()
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
        SpawnRunStatusKind::Waiting => AgentStatus::Waiting {
            reason: run_result.output.clone().unwrap_or_default(),
        },
        SpawnRunStatusKind::Completed => {
            let result = run_result.output.clone().unwrap_or_default();
            if agent_completion_is_interrupted(Some(run_result.finish_reason.as_str())) {
                AgentStatus::Interrupted {
                    partial_result: result,
                    finish_reason: run_result.finish_reason.clone(),
                }
            } else {
                AgentStatus::Completed {
                    result,
                    finish_reason: Some(run_result.finish_reason.clone()),
                }
            }
        }
        SpawnRunStatusKind::Interrupted => AgentStatus::Interrupted {
            partial_result: run_result.output.clone().unwrap_or_default(),
            finish_reason: run_result.finish_reason.clone(),
        },
    }
}

fn fanout_group_title(identity: &AgentFanoutSlotIdentity, title: Option<&str>) -> String {
    title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{} fanout", identity.group_id))
}

fn spawn_run_result_to_sync_output(
    agent_id: String,
    run_id: String,
    run_result: SpawnRunResult,
    duration_ms: u64,
) -> SpawnAgentOutput {
    match spawn_run_status_kind(&run_result.status) {
        SpawnRunStatusKind::Cancelled => {
            let finish_reason = if run_result.finish_reason.trim().is_empty() {
                SPAWN_STATUS_CANCELLED.to_string()
            } else {
                run_result.finish_reason
            };
            SpawnAgentOutput::Cancelled {
                agent_id,
                run_id,
                reason: run_result
                    .output
                    .unwrap_or_else(|| SPAWN_STATUS_CANCELLED.to_string()),
                finish_reason,
                cancelled_by_user: run_result.cancelled_by_user.unwrap_or(false),
                tool_calls: run_result.tool_calls,
                duration_ms,
            }
        }
        SpawnRunStatusKind::Waiting => SpawnAgentOutput::Waiting {
            agent_id,
            run_id,
            reason: run_result.output.unwrap_or_default(),
            tool_calls: run_result.tool_calls,
            duration_ms,
        },
        SpawnRunStatusKind::Failed | SpawnRunStatusKind::Other => SpawnAgentOutput::Failed {
            agent_id,
            run_id,
            error: spawn_run_failure_message(&run_result),
            finish_reason: run_result.finish_reason.clone(),
            duration_ms,
        },
        SpawnRunStatusKind::Completed => SpawnAgentOutput::Completed {
            agent_id,
            run_id,
            result: run_result.output.unwrap_or_default(),
            tool_calls: run_result.tool_calls,
            duration_ms,
        },
        SpawnRunStatusKind::Interrupted => SpawnAgentOutput::Interrupted {
            agent_id,
            run_id,
            result: run_result.output.unwrap_or_default(),
            finish_reason: run_result.finish_reason,
            tool_calls: run_result.tool_calls,
            duration_ms,
        },
    }
}

fn dropped_agent_terminal_output(
    agent_id: &str,
    run_id: &str,
    duration_ms: u64,
) -> SpawnAgentOutput {
    SpawnAgentOutput::Failed {
        agent_id: agent_id.to_string(),
        run_id: run_id.to_string(),
        error: format!(
            "agent executor dropped before returning a terminal result for {agent_id}; \
             the child run was scheduled but no completion payload reached the foreground wait path"
        ),
        finish_reason: "executor_dropped".to_string(),
        duration_ms,
    }
}

fn restored_agent_result_from_journal(
    events: &[astra_services::session_journal::JournalEvent],
    run_id: &str,
) -> Option<String> {
    events.iter().rev().find_map(|event| {
        let item = event.transcript_item.as_ref()?;
        if item.run_id != run_id
            || item.message.get("role").and_then(serde_json::Value::as_str) != Some("assistant")
        {
            return None;
        }
        item.message
            .get("content")
            .and_then(serde_json::Value::as_str)
            .filter(|content| !content.trim().is_empty())
            .map(ToString::to_string)
    })
}

fn restored_agent_status(
    projection: &astra_services::session_workspace::BackgroundLocalAgentTaskProjection,
    exact_result: Option<String>,
) -> AgentStatus {
    let partial = projection.output_tail.clone().unwrap_or_default();
    match projection.status.as_str() {
        "completed" => exact_result.map_or_else(
            || AgentStatus::Interrupted {
                partial_result: partial,
                finish_reason: "canonical_result_unavailable_after_resume".into(),
            },
            |result| AgentStatus::Completed {
                result,
                finish_reason: Some("restored_from_canonical_transcript".into()),
            },
        ),
        "failed" => AgentStatus::Failed {
            error: projection
                .terminal_reason
                .clone()
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or(partial),
            finish_reason: Some("restored_failure".into()),
        },
        "killed" | "cancelled" => AgentStatus::cancelled_anonymous(),
        "interrupted" => AgentStatus::Interrupted {
            partial_result: partial,
            finish_reason: projection
                .terminal_reason
                .clone()
                .unwrap_or_else(|| "restored_interruption".into()),
        },
        "pending" | "running" | "waiting_for_input" => AgentStatus::Interrupted {
            partial_result: partial,
            finish_reason: "local_executor_unavailable_after_resume".into(),
        },
        _ => AgentStatus::Interrupted {
            partial_result: partial,
            finish_reason: "unknown_restored_lifecycle".into(),
        },
    }
}

#[derive(Debug, Clone)]
struct DurableAgentSpawnMetadata {
    agent_id: String,
    agent_type: String,
    description: String,
    fanout_slot: Option<AgentFanoutSlotIdentity>,
}

fn durable_agent_spawn_metadata(
    runs: &[astra_services::runs::DurableRunRecord],
) -> HashMap<String, DurableAgentSpawnMetadata> {
    runs.iter()
        .flat_map(|run| run.events.iter())
        .filter(|event| {
            event.get("type").and_then(serde_json::Value::as_str) == Some("agent_spawned")
        })
        .filter_map(|event| {
            let run_id = event.get("run_id")?.as_str()?.to_string();
            Some((
                run_id,
                DurableAgentSpawnMetadata {
                    agent_id: event.get("agent_id")?.as_str()?.to_string(),
                    agent_type: event
                        .get("agent_type")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("restored")
                        .to_string(),
                    description: event
                        .get("description")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    fanout_slot: event
                        .get("fanout_slot")
                        .cloned()
                        .and_then(|value| serde_json::from_value(value).ok()),
                },
            ))
        })
        .collect()
}

fn durable_run_text(run: &astra_services::runs::DurableRunRecord) -> Option<String> {
    run.events.iter().rev().find_map(|event| {
        (event.get("event_type").and_then(serde_json::Value::as_str) == Some("text_done"))
            .then(|| {
                event
                    .get("data")
                    .and_then(|data| data.get("full_text"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                    .map(ToString::to_string)
            })
            .flatten()
    })
}

fn durable_run_is_terminal(status: &str) -> bool {
    matches!(
        status,
        astra_core::STATUS_COMPLETED | astra_core::STATUS_FAILED | astra_core::STATUS_CANCELLED
    )
}

fn agent_status_is_terminal(status: &AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Completed { .. }
            | AgentStatus::Interrupted { .. }
            | AgentStatus::Failed { .. }
            | AgentStatus::Cancelled { .. }
    )
}

fn durable_agent_status(run: &astra_services::runs::DurableRunRecord) -> AgentStatus {
    let output = durable_run_text(run).unwrap_or_default();
    match run.status.as_str() {
        astra_core::STATUS_COMPLETED if !output.is_empty() => AgentStatus::Completed {
            result: output,
            finish_reason: Some("restored_from_durable_run".into()),
        },
        astra_core::STATUS_COMPLETED => AgentStatus::Interrupted {
            partial_result: String::new(),
            finish_reason: "durable_result_unavailable".into(),
        },
        astra_core::STATUS_FAILED
            if astra_services::coordination::durable_agent_result_is_partial(
                run.error_code.as_deref(),
                run.error_message.as_deref(),
            ) =>
        {
            AgentStatus::Interrupted {
                partial_result: output,
                finish_reason: astra_services::coordination::durable_agent_partial_reason(
                    run.error_code.as_deref(),
                    run.error_message.as_deref(),
                )
                .unwrap_or("partial")
                .to_string(),
            }
        }
        astra_core::STATUS_FAILED => AgentStatus::Failed {
            error: run
                .error_message
                .clone()
                .or_else(|| {
                    run.events.iter().rev().find_map(|event| {
                        event
                            .get("data")?
                            .get("error")?
                            .as_str()
                            .map(ToString::to_string)
                    })
                })
                .unwrap_or_else(|| "durable child failed".into()),
            finish_reason: run.error_code.clone(),
        },
        astra_core::STATUS_CANCELLED => AgentStatus::cancelled_anonymous(),
        astra_core::STATUS_PAUSED => AgentStatus::Interrupted {
            partial_result: output,
            finish_reason: "durable_child_paused".into(),
        },
        astra_core::STATUS_RUNNING | astra_core::STATUS_WAITING => AgentStatus::Waiting {
            reason: run.waiting_for.clone().unwrap_or_else(|| {
                "durable child is owned by another executor or awaiting lease reconciliation".into()
            }),
        },
        other => AgentStatus::Waiting {
            reason: format!("durable child lifecycle '{other}' requires reconciliation"),
        },
    }
}

pub(crate) fn agent_status_to_progress_event(
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
            let reason = agent_finish_reason_text(finish_reason.as_deref());
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
        AgentStatus::Interrupted {
            partial_result,
            finish_reason,
        } => {
            let duration_ms = started_at
                .elapsed()
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            Some(ProgressEventType::Interrupted {
                reason: finish_reason.clone(),
                partial_summary: partial_result.clone(),
                total_tool_calls: metrics.tool_calls,
                total_tokens: (metrics.prompt_tokens, metrics.completion_tokens),
                duration_ms,
            })
        }
        AgentStatus::Failed { error, .. } => Some(ProgressEventType::Failed {
            error: error.clone(),
        }),
        AgentStatus::Waiting { reason } => Some(ProgressEventType::Waiting {
            reason: reason.clone(),
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
    pub inherited_permissions: super::permission_sync::InheritedPermissions,
    /// Skills inherited from the parent agent (subset of parent's active skills).
    pub inherited_skills: Vec<String>,
    /// Optional live-event sink for child token/tool/status mirroring.
    pub live_event_sink: Option<astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
    /// Per-run client lane for executable edge requests. Its ownership is
    /// bounded by the active spawn tree, not the session registry.
    pub client_tool_delivery_tx: Option<tokio::sync::mpsc::Sender<serde_json::Value>>,
    /// DB trace identity shared with the parent Web turn.
    pub trace_context: Option<TraceContext>,
    /// Tool call id of the parent `agent(action='spawn')` invocation.
    pub spawn_tool_call_id: Option<String>,
    /// UI/runtime execution binding metadata inherited by child progress events.
    pub execution_metadata: Option<serde_json::Value>,
    /// Delegation chain from the parent — agent_ids that led to this
    /// child (for circular delegation detection). The child's
    /// `AgenticLoopState` inherits this so subsequent delegations
    /// from the child can detect cycles like A→B→C→A.
    pub delegation_chain: Vec<String>,
}

// ─── Agent Status ───────────────────────────────────────────────────────────

// Re-export from turn-core (canonical definitions live there).
pub use astra_turn_core::orchestration_types::{
    AgentStatus, SpawnedAgentInfo, SpawnedAgentMetrics, agent_completion_is_interrupted,
    agent_finish_reason_text, project_agent_status_to_fanout_slot,
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
    /// Permission mode (auto, bypass, plan, accept_edits, prompt, deny).
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
    /// Producer-owned monotonic lifecycle revision for this agent work unit.
    pub work_revision: u64,
    pub messaging_address: Option<AgentAddress>,
    pub worktree_path: Option<PathBuf>,
    pub started_at: SystemTime,
    pub ended_at: Option<SystemTime>,
    pub metrics: SpawnedAgentMetrics,
    /// Permission summary for this agent.
    pub permission_summary: PermissionSummary,
    pub parent_agent_id: String,
    pub trace_context: Option<TraceContext>,
    pub spawn_tool_call_id: Option<String>,
    pub run_in_background: bool,
    pub fanout_slot: Option<AgentFanoutSlotIdentity>,
    pub execution_metadata: Option<serde_json::Value>,
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
            ended_at: state.ended_at,
            metrics: state.metrics.clone(),
            has_permission_issues: state.metrics.tools_blocked > 0,
            run_in_background: state.run_in_background,
            spawn_tool_call_id: state.spawn_tool_call_id.clone(),
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
    /// Parent control call that owns this launch. Forwarded to the first live
    /// event so provisional UI state can converge on the canonical run before
    /// output begins.
    pub spawn_tool_call_id: Option<String>,
    /// Current nested agent/sub-run depth of the spawned child loop.
    pub recursion_depth: u8,
    /// The agent type (explore, code-review, task, general-purpose).
    pub agent_type: String,
    /// Stable caller-facing label for durable run trees and work surfaces.
    pub description: String,
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
    pub inherited_permissions: super::permission_sync::InheritedPermissions,
    /// Parent agent address for permission requests (if this is a child agent).
    pub parent_address: Option<astra_messaging::types::AgentAddress>,
    /// Permission context for runtime permission management.
    /// Created from the explicit inherited permissions envelope.
    pub permission_context:
        std::sync::Arc<tokio::sync::RwLock<super::permission_sync::PermissionSyncContext>>,
    /// Skills inherited from parent agent.
    pub inherited_skills: Vec<String>,
    /// Optional live-event sink for child token/tool/status mirroring.
    pub live_event_sink: Option<astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
    /// Client delivery lane inherited from the active parent run.
    pub client_tool_delivery_tx: Option<tokio::sync::mpsc::Sender<serde_json::Value>>,
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
    /// UI/runtime execution binding metadata inherited from the parent run.
    pub execution_metadata: Option<serde_json::Value>,
    /// True when this child inherited a parent prefix (fork mode).
    /// Fork children are NOT allowed to fork again — prevents
    /// degenerate case where a forked child forks with its own prefix
    /// (which differs from the original parent's, breaking the cache
    /// reuse chain). Same semantics as the reference agent's `isInForkChild()`.
    pub is_fork_child: bool,
    /// Delegation chain from the parent — agent_ids that led to this
    /// child (for circular delegation detection). The child's
    /// `AgenticLoopState` inherits this so subsequent delegations
    /// from the child can detect cycles like A→B→C→A.
    pub delegation_chain: Vec<String>,
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
            .field("description", &self.description)
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
    /// Structured cancellation source. `Some(true)` means the user explicitly
    /// cancelled the child; `Some(false)` means runtime/system cancellation.
    /// `None` means the run did not end in the cancelled state.
    pub cancelled_by_user: Option<bool>,
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
    /// Number of agentic-loop rounds completed by this child run.
    pub turns_completed: u32,
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

    /// Cancel executor-owned control and durable state before the spawner
    /// aborts the task future. Implementations that only execute in-memory
    /// test work may keep the default no-op; server executors use this hook to
    /// cancel remote tools and CAS the child run to a terminal state.
    async fn cancel_spawned_run(
        &self,
        _run_id: &str,
        _user_id: Option<&str>,
        _reason: &str,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Bind a parent session after the executor has been installed.
    ///
    /// Interactive clients can learn the server session from the first streamed
    /// `session_info` event, after the per-turn spawner already exists but
    /// before any child spawn executes. Executors that persist child transcript
    /// state should use this as the same binding as construction-time session
    /// setup. Stateless executors can ignore it.
    fn bind_parent_session(&self, _session_id: &str) {}
}

/// Loads a bounded authoritative session snapshot for agents observed from a
/// different executor. Implementations own coalescing/backoff so fanout reads
/// never degrade into per-agent database polling.
#[async_trait]
pub trait DurableAgentReconciler: Send + Sync {
    async fn load_agent_recovery(
        &self,
    ) -> Result<Vec<astra_services::runs::DurableRunRecord>, String>;
}

// ─── Dynamic Agent Spawner ──────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CancelOrigin {
    User,
    System,
}

/// Handles dynamic agent creation at runtime.
///
/// This is the core component that allows LLMs to spawn sub-agents without
/// pre-defined team configurations.
pub struct DynamicAgentSpawner {
    /// For inter-agent messaging.
    mailbox_router: Arc<AgentMailboxRouter>,
    /// For tracking spawned agents.
    active_agents: Arc<RwLock<HashMap<String, SpawnedAgentState>>>,
    /// Parent run ids whose descendant trees are being or have been
    /// cancelled. Spawn reservation takes a read fence before inserting;
    /// cancellation takes the write fence before its authoritative snapshot.
    cancelling_parent_runs: Arc<RwLock<HashSet<String>>>,
    /// Progress event broadcaster.
    progress_broadcaster: Arc<ProgressBroadcaster>,
    /// Shared context cache for cross-agent knowledge sharing.
    context_cache: Arc<SharedContextCache>,
    /// Optional executor for running agents (provided by CLI layer).
    executor: Option<Arc<dyn SpawnAgentExecutor>>,
    /// Optional session ID for persisting agent state to journal.
    session_id: Arc<std::sync::RwLock<Option<String>>>,
    /// Agent type registry (builtins + user-defined).
    agent_registry: astra_turn_core::orchestration_team_config::AgentRegistry,
    /// Completed agents archive for history queries.
    completed_agents: Arc<RwLock<VecDeque<SpawnedAgentState>>>,
    /// Strong ownership of the task supervisor exists only on the
    /// session/root spawner. Task-side handles deliberately keep this as
    /// `None`: a task must never keep alive the JoinSet that owns that same
    /// task.
    _background_task_owner: Option<Arc<std::sync::Mutex<tokio::task::JoinSet<()>>>>,
    /// Weak task-side route into the root-owned supervisor. Nested agents can
    /// still register with the same JoinSet while the session owner exists,
    /// but dropping the owner deterministically aborts every remaining task.
    background_tasks: std::sync::Weak<std::sync::Mutex<tokio::task::JoinSet<()>>>,
    /// Spawn setup holds a read permit until its host future is registered.
    /// Shutdown takes the write permit, closes admission, and then takes the
    /// JoinSet, so no child can land in an unobserved replacement set.
    background_task_admission: Arc<tokio::sync::RwLock<bool>>,
    /// Per-agent abort handles for background children so the parent can cancel
    /// a single lagging sub-agent without killing siblings.
    background_abort_handles: Arc<RwLock<HashMap<String, tokio::task::AbortHandle>>>,
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
    /// Reverse index for fanout lookup by child agent id. Kept separate from
    /// the pure fanout projection so runtime queries avoid scanning every
    /// group and slot.
    fanout_agent_index: Arc<RwLock<HashMap<String, String>>>,
    /// Stable default `get_results` payload for terminal groups. Tool batches
    /// are sequential, so an identical second control read reuses this result
    /// instead of repeating child-result collection and durable reads.
    fanout_terminal_result_cache: Arc<RwLock<HashMap<String, String>>>,
    /// Cached count of active fanout slots (running or waiting for input). Derived from
    /// `fanout_groups`; state-transition paths update it for cheap telemetry.
    ///
    /// Updated atomically on every state transition for cheap fanout
    /// telemetry. Call `repair_fanout_slot_count` to recompute from
    /// authoritative state after crash recovery or poison.
    cached_active_fanout_slots: Arc<std::sync::atomic::AtomicUsize>,
    /// Optional Server-only durable refresher. CLI/Edge local executors leave
    /// this unset because their task registry is updated by the executor.
    durable_reconciler: Arc<RwLock<Option<Arc<dyn DurableAgentReconciler>>>>,
    /// Agents reconstructed as read-only observations of another executor.
    /// Only these states may be replaced by durable reconciliation.
    durable_observed_agent_ids: Arc<RwLock<HashSet<String>>>,
    durable_reconcile_lock: Arc<tokio::sync::Mutex<()>>,
    /// Session-scoped typed work authority shared by CLI and Server model
    /// boundaries. Fanout mutations publish here at the producer seam.
    active_work_registry: Option<Arc<astra_core::work_unit::ActiveWorkRegistry>>,
}

impl DynamicAgentSpawner {
    /// Create a new spawner with the given dependencies.
    pub fn new(mailbox_router: Arc<AgentMailboxRouter>) -> Self {
        let background_task_owner = Arc::new(std::sync::Mutex::new(tokio::task::JoinSet::new()));
        Self {
            mailbox_router,
            active_agents: Arc::new(RwLock::new(HashMap::new())),
            cancelling_parent_runs: Arc::new(RwLock::new(HashSet::new())),
            progress_broadcaster: Arc::new(ProgressBroadcaster::default()),
            context_cache: Arc::new(SharedContextCache::default()),
            executor: None,
            session_id: Arc::new(std::sync::RwLock::new(None)),
            agent_registry:
                astra_turn_core::orchestration_team_config::AgentRegistry::builtins_only(),
            completed_agents: Arc::new(RwLock::new(VecDeque::new())),
            background_tasks: Arc::downgrade(&background_task_owner),
            _background_task_owner: Some(background_task_owner),
            background_task_admission: Arc::new(tokio::sync::RwLock::new(true)),
            background_abort_handles: Arc::new(RwLock::new(HashMap::new())),
            completion_notifiers: Arc::new(RwLock::new(HashMap::new())),
            foreground_promotion_requests: Arc::new(RwLock::new(HashSet::new())),
            prefix_store: None,
            prefix_resolve_outcomes: Arc::new(RwLock::new(HashMap::new())),
            trace_writer: None,
            max_concurrent_agents: None,
            fanout_groups: Arc::new(RwLock::new(HashMap::new())),
            fanout_agent_index: Arc::new(RwLock::new(HashMap::new())),
            fanout_terminal_result_cache: Arc::new(RwLock::new(HashMap::new())),
            cached_active_fanout_slots: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            durable_reconciler: Arc::new(RwLock::new(None)),
            durable_observed_agent_ids: Arc::new(RwLock::new(HashSet::new())),
            durable_reconcile_lock: Arc::new(tokio::sync::Mutex::new(())),
            active_work_registry: None,
        }
    }

    pub async fn set_durable_agent_reconciler(&self, reconciler: Arc<dyn DurableAgentReconciler>) {
        *self.durable_reconciler.write().await = Some(reconciler);
    }

    /// Refresh only read-only, remotely-owned observations. Locally executing
    /// child state is never overwritten by a database snapshot.
    pub async fn reconcile_durable_agent_runs(&self) -> Result<usize, String> {
        let _reconcile_guard = self.durable_reconcile_lock.lock().await;
        let Some(reconciler) = self.durable_reconciler.read().await.clone() else {
            return Ok(0);
        };
        let runs = reconciler.load_agent_recovery().await?;
        if runs.is_empty() {
            return Ok(0);
        }
        let restored = self.restore_durable_agent_runs(&runs).await;
        let spawned = durable_agent_spawn_metadata(&runs);
        let observed = self.durable_observed_agent_ids.read().await.clone();
        let mut changed = Vec::new();
        {
            let mut completed = self.completed_agents.write().await;
            for run in runs.iter().filter(|run| run.depth > 0) {
                let Some(agent_id) = run.agent_id.as_deref().or_else(|| {
                    spawned
                        .get(run.run_id.as_str())
                        .map(|spawn| spawn.agent_id.as_str())
                }) else {
                    continue;
                };
                if !observed.contains(agent_id) {
                    continue;
                }
                let Some(state) = completed
                    .iter_mut()
                    .find(|state| state.agent_id == agent_id)
                else {
                    continue;
                };
                let status = durable_agent_status(run);
                if state.status == status {
                    continue;
                }
                state.status = status;
                state.work_revision = state.work_revision.saturating_add(1);
                state.ended_at = durable_run_is_terminal(&run.status).then(SystemTime::now);
                state.metrics.tool_calls = run.total_tool_calls;
                state.metrics.prompt_tokens = run.total_prompt_tokens;
                state.metrics.completion_tokens = run.total_completion_tokens;
                changed.push(state.clone());
            }
        }
        for state in &changed {
            self.publish_background_agent(state);
            if agent_status_is_terminal(&state.status) {
                self.record_fanout_terminal_state(state).await;
                self.notify_completion(&state.agent_id).await;
            }
        }
        Ok(restored + changed.len())
    }

    /// Rebuild read-only local agent/fanout results after a CLI or Edge
    /// process restart. Workspace rows provide immutable run lineage and
    /// lifecycle; the canonical journal supplies a complete final assistant
    /// result when one exists. A non-terminal row whose executor disappeared
    /// is restored as interrupted partial work, never as a fake running task.
    pub async fn restore_workspace_agent_projections(
        &self,
        projections: &[astra_services::session_workspace::BackgroundLocalAgentTaskProjection],
    ) -> usize {
        let journal_events = self
            .current_session_id()
            .as_deref()
            .and_then(|session_id| astra_services::session_journal::read_journal(session_id).ok())
            .unwrap_or_default();
        let mut restored = 0;
        for projection in projections {
            if self.get_agent_state_any(&projection.id).await.is_some() {
                continue;
            }
            let exact_result =
                restored_agent_result_from_journal(&journal_events, projection.run_id.as_str());
            let status = restored_agent_status(projection, exact_result);
            let fanout_slot = projection.fanout.as_ref().and_then(|fanout| {
                AgentFanoutSlotIdentity::new(
                    fanout.group_id.clone(),
                    fanout.target_count,
                    fanout.slot_index,
                    Some(fanout.slot_label.clone()),
                )
                .ok()
            });
            let started_at = std::time::UNIX_EPOCH
                .checked_add(std::time::Duration::from_millis(projection.started_at_ms))
                .unwrap_or(std::time::UNIX_EPOCH);
            let ended_at = projection.ended_at_ms.and_then(|millis| {
                std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_millis(millis))
            });
            let state = SpawnedAgentState {
                agent_id: projection.id.clone(),
                run_id: projection.run_id.clone(),
                parent_run_id: projection.parent_run_id.clone(),
                agent_type: "restored".into(),
                description: projection.title.clone(),
                status,
                work_revision: 2,
                messaging_address: None,
                worktree_path: None,
                started_at,
                ended_at: ended_at.or(Some(SystemTime::now())),
                metrics: SpawnedAgentMetrics::default(),
                permission_summary: PermissionSummary::default(),
                parent_agent_id: "root".into(),
                trace_context: None,
                spawn_tool_call_id: None,
                run_in_background: true,
                fanout_slot: fanout_slot.clone(),
                execution_metadata: None,
            };
            if let (Some(identity), Some(fanout)) =
                (fanout_slot.as_ref(), projection.fanout.as_ref())
            {
                if let Err(error) = self
                    .record_fanout_spawn_accepted(
                        identity,
                        Some(fanout.group_title.as_str()),
                        &state.agent_id,
                        &state.run_id,
                        &state.agent_type,
                        &state.description,
                        None,
                        &state.parent_run_id,
                    )
                    .await
                {
                    tracing::warn!(
                        agent_id = %state.agent_id,
                        run_id = %state.run_id,
                        group_id = %identity.group_id,
                        %error,
                        "could not restore fanout membership; preserving the child result without group membership"
                    );
                } else {
                    self.record_fanout_terminal_state(&state).await;
                }
            }
            self.publish_background_agent(&state);
            self.archive_state(state).await;
            restored += 1;
        }
        restored
    }

    /// Rebuild the session-scoped registry on a new Server pod from the
    /// authoritative bounded run snapshot. This is read-only recovery: live
    /// ownership remains in `agent_runs.owner_pod_id`/lease; the reconstructed
    /// spawner only makes existing results and partial fan-in observable.
    pub async fn restore_durable_agent_runs(
        &self,
        runs: &[astra_services::runs::DurableRunRecord],
    ) -> usize {
        let spawned = durable_agent_spawn_metadata(runs);
        let mut restored = 0;
        for run in runs.iter().filter(|run| run.depth > 0) {
            let Some(agent_id) = run.agent_id.as_deref().or_else(|| {
                spawned
                    .get(run.run_id.as_str())
                    .map(|spawn| spawn.agent_id.as_str())
            }) else {
                continue;
            };
            if self.get_agent_state_any(agent_id).await.is_some() {
                continue;
            }
            let spawn = spawned.get(run.run_id.as_str());
            let status = durable_agent_status(run);
            let state = SpawnedAgentState {
                agent_id: agent_id.to_string(),
                run_id: run.run_id.clone(),
                parent_run_id: run
                    .parent_run_id
                    .clone()
                    .unwrap_or_else(|| ROOT_RUN_ID.into()),
                agent_type: spawn
                    .map(|spawn| spawn.agent_type.clone())
                    .unwrap_or_else(|| "restored".into()),
                description: spawn
                    .map(|spawn| spawn.description.clone())
                    .or_else(|| run.agent_binding_name.clone())
                    .unwrap_or_else(|| agent_id.to_string()),
                status,
                work_revision: run.run_generation.max(1),
                messaging_address: None,
                worktree_path: None,
                started_at: SystemTime::now(),
                ended_at: durable_run_is_terminal(&run.status).then(SystemTime::now),
                metrics: SpawnedAgentMetrics {
                    tool_calls: run.total_tool_calls,
                    prompt_tokens: run.total_prompt_tokens,
                    completion_tokens: run.total_completion_tokens,
                    ..Default::default()
                },
                permission_summary: PermissionSummary::default(),
                parent_agent_id: "root".into(),
                trace_context: None,
                spawn_tool_call_id: None,
                run_in_background: true,
                fanout_slot: spawn.and_then(|spawn| spawn.fanout_slot.clone()),
                execution_metadata: None,
            };
            if let Some(identity) = state.fanout_slot.as_ref() {
                if self
                    .record_fanout_spawn_accepted(
                        identity,
                        Some(identity.group_id.as_str()),
                        &state.agent_id,
                        &state.run_id,
                        &state.agent_type,
                        &state.description,
                        None,
                        &state.parent_run_id,
                    )
                    .await
                    .is_ok()
                    && agent_status_is_terminal(&state.status)
                {
                    self.record_fanout_terminal_state(&state).await;
                }
            }
            self.durable_observed_agent_ids
                .write()
                .await
                .insert(state.agent_id.clone());
            self.publish_background_agent(&state);
            self.archive_state(state).await;
            restored += 1;
        }
        restored
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
        // Builder ordering must not decide whether child transcript state is
        // session-bound. Keep the callback outside the session lock just like
        // late binding below.
        if let Some(session_id) = self.current_session_id() {
            executor.bind_parent_session(&session_id);
        }
        self.executor = Some(executor);
        self
    }

    pub fn with_active_work_registry(
        mut self,
        registry: Arc<astra_core::work_unit::ActiveWorkRegistry>,
    ) -> Self {
        self.active_work_registry = Some(registry);
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

    /// Snapshot every currently active fanout as producer-owned work truth.
    ///
    /// This is the session-turn boundary API. Callers should not route the UI
    /// task-list rendering back into model context or reconstruct group state
    /// from individual child histories.
    pub async fn active_fanout_work_unit_observations(
        &self,
    ) -> Vec<astra_core::work_unit::WorkUnitObservation> {
        self.list_fanout_groups()
            .await
            .into_iter()
            .filter(|group| !group.is_terminal())
            .filter_map(|group| group.work_unit_observation())
            .collect()
    }

    fn publish_fanout_group(&self, group: &AgentFanoutGroupProjection) {
        let Some(registry) = self.active_work_registry.as_ref() else {
            return;
        };
        if let Some(observation) = group.work_unit_observation() {
            registry.observe(&observation);
        }
    }

    fn publish_background_agent(&self, state: &SpawnedAgentState) {
        use astra_core::work_unit::{
            WorkUnitObservation, WorkUnitObservationMode, WorkUnitStatus, WorkUnitWakePolicy,
        };

        let Some(registry) = self.active_work_registry.as_ref() else {
            return;
        };
        // Fanout children are represented by exactly one group work unit.
        if !state.run_in_background || state.fanout_slot.is_some() {
            return;
        }
        let status = match &state.status {
            AgentStatus::Initializing => WorkUnitStatus::Pending,
            AgentStatus::Running { .. } | AgentStatus::Idle => WorkUnitStatus::Running,
            AgentStatus::Waiting { .. } => WorkUnitStatus::WaitingForInput,
            AgentStatus::Completed { .. } => WorkUnitStatus::Completed,
            AgentStatus::Interrupted { .. } => WorkUnitStatus::Interrupted,
            AgentStatus::Failed { .. } => WorkUnitStatus::Failed,
            AgentStatus::Cancelled { .. } => WorkUnitStatus::Cancelled,
        };
        let Some(observation) = WorkUnitObservation::new(
            state.agent_id.clone(),
            "agent",
            status,
            state.work_revision.max(1),
            WorkUnitObservationMode::Current,
        ) else {
            return;
        };
        registry.observe(&observation.with_wake_policy(WorkUnitWakePolicy::OnAttentionOrTerminal));
    }

    pub async fn declare_fanout_group(
        &self,
        group_id: &str,
        title: &str,
        target_count: usize,
        created_by_tool_use_id: Option<&str>,
        parent_run_id: &str,
    ) -> Result<(), SpawnError> {
        let identity = AgentFanoutSlotIdentity::new(group_id, target_count, 0, None)
            .map_err(SpawnError::InvalidInput)?;
        let (mut groups, evicted_agent_ids) = self
            .get_or_validate_fanout_group(
                &identity,
                Some(title),
                created_by_tool_use_id,
                parent_run_id,
            )
            .await?;
        let mut index = self.fanout_agent_index.write().await;
        for evicted_agent_id in &evicted_agent_ids {
            index.remove(evicted_agent_id);
        }
        if let Some(group) = groups.get_mut(group_id) {
            group.touch();
            self.publish_fanout_group(group);
        }
        Ok(())
    }

    pub async fn fanout_group_for_agent(
        &self,
        agent_id: &str,
    ) -> Option<AgentFanoutGroupProjection> {
        let group_id = self
            .fanout_agent_index
            .read()
            .await
            .get(agent_id)
            .cloned()?;
        self.fanout_groups.read().await.get(&group_id).cloned()
    }

    pub async fn cached_terminal_fanout_result(&self, group_id: &str) -> Option<String> {
        self.fanout_terminal_result_cache
            .read()
            .await
            .get(group_id)
            .cloned()
    }

    pub async fn cache_terminal_fanout_result(&self, group_id: &str, result: String) {
        let groups = self.fanout_groups.read().await;
        if !groups
            .get(group_id)
            .is_some_and(AgentFanoutGroupProjection::is_terminal)
        {
            return;
        }
        let mut cache = self.fanout_terminal_result_cache.write().await;
        cache.retain(|cached_group_id, _| groups.contains_key(cached_group_id));
        cache.insert(group_id.to_string(), result);
    }

    fn reap_finished_agent_tasks(&self) {
        let Some(tasks) = self.background_tasks.upgrade() else {
            return;
        };
        let Ok(mut tasks) = tasks.lock() else {
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

    async fn cleanup_worktree(&self, worktree_path: Option<PathBuf>, agent_id: &str) {
        let Some(path) = worktree_path else {
            return;
        };
        let agent_id = agent_id.to_string();
        let cleanup_agent_id = agent_id.clone();
        if let Err(error) = tokio::task::spawn_blocking(move || {
            cleanup_agent_worktree(Some(&path), &cleanup_agent_id);
        })
        .await
        {
            astra_core::agent_warn!(
                "spawner",
                "worktree cleanup task for {agent_id} failed to join: {error}"
            );
        }
    }

    async fn take_foreground_promotion_request(&self, agent_id: &str) -> bool {
        self.foreground_promotion_requests
            .write()
            .await
            .remove(agent_id)
    }

    /// Promote the newest foreground work item for `parent_run_id` into
    /// background mode. A direct agent is promoted alone; a fanout slot
    /// promotes every still-foreground slot in the same group atomically.
    /// Each waiting spawn wakes and returns `Launched`, so a multi-slot
    /// `agent_fanout.start` cannot be left half foreground after Ctrl+B.
    pub async fn promote_foreground_work_to_background(
        &self,
        parent_run_id: Option<&str>,
    ) -> Vec<SpawnedAgentInfo> {
        let promoted = {
            let mut active_agents = self.active_agents.write().await;
            let Some((selected_agent_id, selected_parent_run_id, selected_group_id)) =
                active_agents
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
                    .map(|(agent_id, state)| {
                        (
                            agent_id.clone(),
                            state.parent_run_id.clone(),
                            state.fanout_slot.as_ref().map(|slot| slot.group_id.clone()),
                        )
                    })
            else {
                return Vec::new();
            };

            let mut agent_ids = active_agents
                .iter()
                .filter(|(agent_id, state)| {
                    if state.run_in_background || state.parent_run_id != selected_parent_run_id {
                        return false;
                    }
                    match selected_group_id.as_deref() {
                        Some(group_id) => state
                            .fanout_slot
                            .as_ref()
                            .is_some_and(|slot| slot.group_id == group_id),
                        None => agent_id.as_str() == selected_agent_id,
                    }
                })
                .map(|(agent_id, state)| {
                    (
                        state
                            .fanout_slot
                            .as_ref()
                            .map(|slot| slot.slot_index)
                            .unwrap_or(usize::MAX),
                        agent_id.clone(),
                    )
                })
                .collect::<Vec<_>>();
            agent_ids.sort_by_key(|(slot_index, _)| *slot_index);
            agent_ids
                .into_iter()
                .filter_map(|(_, agent_id)| {
                    let state = active_agents.get_mut(&agent_id)?;
                    state.run_in_background = true;
                    Some(SpawnedAgentInfo::from(&*state))
                })
                .collect::<Vec<_>>()
        };

        {
            let mut requests = self.foreground_promotion_requests.write().await;
            requests.extend(promoted.iter().map(|agent| agent.agent_id.clone()));
        }
        let notifiers = self.completion_notifiers.read().await;
        for agent in &promoted {
            if let Some(notifier) = notifiers.get(&agent.agent_id) {
                notifier.notify_waiters();
            }
        }
        promoted
    }

    /// Helper to get or create a fanout group and validate it's not terminal.
    /// Returns the group entry and any evicted agent IDs.
    async fn get_or_validate_fanout_group(
        &self,
        identity: &AgentFanoutSlotIdentity,
        group_title: Option<&str>,
        created_by_tool_use_id: Option<&str>,
        parent_run_id: &str,
    ) -> Result<
        (
            tokio::sync::RwLockWriteGuard<'_, HashMap<String, AgentFanoutGroupProjection>>,
            Vec<String>,
        ),
        SpawnError,
    > {
        let mut groups = self.fanout_groups.write().await;
        let is_new = !groups.contains_key(&identity.group_id);
        let evicted_agent_ids = if is_new {
            if let Some(existing) = groups
                .values()
                .find(|group| group.parent_run_id.as_deref() == Some(parent_run_id))
            {
                return Err(SpawnError::InvalidInput(format!(
                    "parent run '{parent_run_id}' already owns fanout group '{}' with fixed target_count {}; a parent run may start only one fanout group",
                    existing.group_id, existing.target_count
                )));
            }
            self.evict_terminal_fanout_group_if_full(&mut groups)?
        } else {
            Vec::new()
        };
        let group = groups.entry(identity.group_id.clone()).or_insert_with(|| {
            let mut group = AgentFanoutGroupProjection::new(
                identity.group_id.clone(),
                fanout_group_title(identity, group_title),
                identity.target_count,
            );
            group.created_by_tool_use_id = created_by_tool_use_id.map(ToString::to_string);
            group.parent_run_id = Some(parent_run_id.to_string());
            group
        });
        match group.parent_run_id.as_deref() {
            Some(existing_parent_run_id) if existing_parent_run_id != parent_run_id => {
                return Err(SpawnError::InvalidInput(format!(
                    "fanout group '{}' belongs to parent_run_id '{}', not '{}'",
                    identity.group_id, existing_parent_run_id, parent_run_id
                )));
            }
            None => {
                group.parent_run_id = Some(parent_run_id.to_string());
            }
            Some(_) => {}
        }
        if group.target_count != identity.target_count {
            return Err(SpawnError::InvalidInput(format!(
                "fanout group '{}' target_count changed from {} to {}",
                identity.group_id, group.target_count, identity.target_count
            )));
        }
        // Reject reuse of terminal groups (Finished or Incomplete) —
        // LLM must create a new group_id for retries rather than
        // appending to a settled group, which would corrupt the
        // fixed-size accounting.
        if matches!(
            group.status,
            AgentFanoutStatus::Finished | AgentFanoutStatus::Incomplete
        ) {
            let status_label = match group.status {
                AgentFanoutStatus::Finished => "finished",
                AgentFanoutStatus::Incomplete => "incomplete",
                _ => unreachable!(),
            };
            return Err(SpawnError::InvalidInput(format!(
                "fanout group '{}' is already {status_label} (all {} slots settled); create a new group_id for retries",
                identity.group_id, group.target_count
            )));
        }
        Ok((groups, evicted_agent_ids))
    }

    async fn record_fanout_spawn_accepted(
        &self,
        identity: &AgentFanoutSlotIdentity,
        group_title: Option<&str>,
        agent_id: &str,
        run_id: &str,
        agent_type: &str,
        description: &str,
        created_by_tool_use_id: Option<&str>,
        parent_run_id: &str,
    ) -> Result<(), SpawnError> {
        let (mut groups, evicted_agent_ids) = self
            .get_or_validate_fanout_group(
                identity,
                group_title,
                created_by_tool_use_id,
                parent_run_id,
            )
            .await?;
        // Acquire the index lock while still holding `groups` to close the
        // TOCTOU window: no concurrent eviction can race between our group
        // mutation and our index update.  Lock ordering: groups → index
        // (consistent across all call sites, deadlock-free).
        let mut index = self.fanout_agent_index.write().await;
        // Clean up evicted agent IDs unconditionally — this must happen
        // even if the slot operations below fail, because the eviction
        // has already removed the group from the map.
        for evicted_agent_id in &evicted_agent_ids {
            index.remove(evicted_agent_id);
        }
        let group = groups.get_mut(&identity.group_id).ok_or_else(|| {
            SpawnError::Race(format!(
                "fanout group '{}' disappeared while its write guard was held",
                identity.group_id
            ))
        })?;
        let active_before = group.summary().active;
        group
            .set_slot_request(
                identity.slot_index,
                identity.slot_id.clone(),
                agent_type,
                description,
            )
            .map_err(SpawnError::InvalidInput)?;
        group
            .record_spawn_accepted_with_run(identity.slot_index, agent_id, Some(run_id.to_string()))
            .map_err(SpawnError::InvalidInput)?;
        let active_after = group.summary().active;
        group.touch();
        self.fanout_terminal_result_cache
            .write()
            .await
            .remove(&identity.group_id);
        self.adjust_cached_active_fanout_slots(active_before, active_after);
        index.insert(agent_id.to_string(), identity.group_id.clone());
        self.publish_fanout_group(group);
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
        parent_run_id: &str,
    ) -> Result<(), SpawnError> {
        let (mut groups, evicted_agent_ids) = self
            .get_or_validate_fanout_group(
                identity,
                group_title,
                created_by_tool_use_id,
                parent_run_id,
            )
            .await?;
        // Acquire index lock while still holding `groups` to close the
        // TOCTOU window (see `record_fanout_spawn_accepted`).
        let mut index = self.fanout_agent_index.write().await;
        for evicted_agent_id in &evicted_agent_ids {
            index.remove(evicted_agent_id);
        }
        let group = groups.get_mut(&identity.group_id).ok_or_else(|| {
            SpawnError::Race(format!(
                "fanout group '{}' disappeared while its write guard was held",
                identity.group_id
            ))
        })?;
        let active_before = group.summary().active;
        group
            .set_slot_request(
                identity.slot_index,
                identity.slot_id.clone(),
                agent_type,
                description,
            )
            .map_err(SpawnError::InvalidInput)?;
        group
            .record_spawn_rejected(identity.slot_index, reason)
            .map_err(SpawnError::InvalidInput)?;
        let active_after = group.summary().active;
        group.touch();
        self.adjust_cached_active_fanout_slots(active_before, active_after);
        self.publish_fanout_group(group);
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
                    &context.parent_run_id,
                )
                .await;
        }
    }

    pub async fn fanout_group_for_parent_run(
        &self,
        parent_run_id: &str,
    ) -> Option<AgentFanoutGroupProjection> {
        let groups = self.fanout_groups.read().await;
        groups
            .values()
            .find(|group| group.parent_run_id.as_deref() == Some(parent_run_id))
            .cloned()
    }

    async fn record_fanout_terminal_state(&self, state: &SpawnedAgentState) {
        let Some(identity) = state.fanout_slot.as_ref() else {
            return;
        };
        let projection = project_agent_status_to_fanout_slot(&state.status);
        let status = projection.status;
        let reason = projection.terminal_reason;
        let terminal_reason_label = reason.as_deref().unwrap_or("").to_string();
        let mut groups = self.fanout_groups.write().await;
        let Some(group) = groups.get_mut(&identity.group_id) else {
            tracing::warn!(
                target: "fanout",
                agent_id = %state.agent_id,
                group_id = %identity.group_id,
                "record fanout terminal skipped: group evicted before terminal update",
            );
            return;
        };
        let active_before = group.summary().active;
        if let Err(error) = group.record_status_by_agent(&state.agent_id, status, reason) {
            tracing::warn!(
                target: "fanout",
                agent_id = %state.agent_id,
                group_id = %identity.group_id,
                error = %error,
                "record fanout lifecycle state failed; skipping touch/budget adjust",
            );
            return;
        }
        let active_after = group.summary().active;
        group.touch();
        self.publish_fanout_group(group);
        self.fanout_terminal_result_cache
            .write()
            .await
            .remove(&identity.group_id);
        tracing::info!(
            target: "fanout",
            group_id = %identity.group_id,
            slot_index = identity.slot_index,
            slot_id = identity.slot_id.as_deref().unwrap_or(""),
            agent_id = %state.agent_id,
            child_run_id = %state.run_id,
            canonical_agent_status = %Self::agent_status_trace_label(&state.status),
            fanout_slot_status = %status.as_str(),
            terminal_reason = %terminal_reason_label,
            active_before,
            active_after,
            "fanout slot reached terminal state"
        );
        drop(groups);
        self.adjust_cached_active_fanout_slots(active_before, active_after);
    }

    /// Evict the least-recently-touched terminal group when the fanout-groups
    /// map is at capacity.  Only terminal groups (Finished / Incomplete) are
    /// candidates — evicting a live group would corrupt in-flight agent
    /// accounting.  Returns `Err(FanoutGroupLimitExceeded)` when at capacity
    /// and no terminal candidate is available, so the caller fails loudly
    /// instead of leaking memory through unbounded growth.
    fn evict_terminal_fanout_group_if_full(
        &self,
        groups: &mut HashMap<String, AgentFanoutGroupProjection>,
    ) -> Result<Vec<String>, SpawnError> {
        if groups.len() < MAX_FANOUT_GROUPS {
            return Ok(Vec::new());
        }
        // Find the terminal group with the oldest last_touched.
        let Some((evict_id, _)) = groups
            .iter()
            .filter(|(_, g)| g.is_terminal())
            .min_by_key(|(_, g)| g.last_touched)
        else {
            return Err(SpawnError::FanoutGroupLimitExceeded {
                active: groups.len(),
                limit: MAX_FANOUT_GROUPS,
            });
        };
        let evict_id = evict_id.clone();
        Ok(groups
            .remove(&evict_id)
            .map(|group| {
                group
                    .slots
                    .into_iter()
                    .filter_map(|slot| slot.agent_id)
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Recompute `cached_active_fanout_slots` from the authoritative
    /// fanout-groups state.  Call this after crash recovery, poison
    /// recovery, or any path where the cache may have drifted.
    ///
    /// Returns the recomputed count.
    pub async fn repair_fanout_slot_count(&self) -> usize {
        let count = self.count_active_fanout_slots_from_groups().await;
        self.cached_active_fanout_slots
            .store(count, std::sync::atomic::Ordering::SeqCst);
        count
    }

    fn adjust_cached_active_fanout_slots(&self, active_before: usize, active_after: usize) {
        match active_after.cmp(&active_before) {
            std::cmp::Ordering::Greater => {
                self.cached_active_fanout_slots.fetch_add(
                    active_after - active_before,
                    std::sync::atomic::Ordering::SeqCst,
                );
            }
            std::cmp::Ordering::Less => {
                let delta = active_before - active_after;
                self.cached_active_fanout_slots
                    .fetch_update(
                        std::sync::atomic::Ordering::SeqCst,
                        std::sync::atomic::Ordering::SeqCst,
                        |current| Some(current.saturating_sub(delta)),
                    )
                    .ok();
            }
            std::cmp::Ordering::Equal => {}
        }
    }

    /// Authoritative count: number of non-terminal accepted slots across
    /// all non-terminal fanout groups.  This is the single source of
    /// truth — `cached_active_fanout_slots` is just a performance
    /// optimization derived from this computation.
    async fn count_active_fanout_slots_from_groups(&self) -> usize {
        let groups = self.fanout_groups.read().await;
        groups
            .values()
            .filter(|g| !g.is_terminal())
            .flat_map(|g| g.slots.iter())
            .filter(|s| {
                matches!(
                    s.status,
                    AgentFanoutSlotStatus::Running | AgentFanoutSlotStatus::WaitingForInput
                )
            })
            .count()
    }

    async fn mark_fanout_result_collected(&self, state: &SpawnedAgentState) {
        let Some(identity) = state.fanout_slot.as_ref() else {
            return;
        };
        let mut groups = self.fanout_groups.write().await;
        if let Some(group) = groups.get_mut(&identity.group_id) {
            group.mark_result_collected(&state.agent_id);
            group.touch();
            self.publish_fanout_group(group);
        }
    }

    /// Enable journal persistence for agent lifecycle events.
    pub fn with_session(self, session_id: String) -> Self {
        self.bind_session(session_id);
        self
    }

    /// Late-bind the parent session for lifecycle journaling and executor-owned
    /// child transcript persistence.
    pub fn bind_session(&self, session_id: impl Into<String>) {
        let session_id = session_id.into();
        if session_id.trim().is_empty() {
            return;
        }
        let changed = {
            let mut guard = self
                .session_id
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if guard.as_deref() == Some(session_id.as_str()) {
                false
            } else {
                *guard = Some(session_id.clone());
                true
            }
        };
        if !changed {
            return;
        }
        // Never invoke executor-owned code while holding the session lock.
        // Besides keeping the synchronous critical section bounded, this
        // permits implementations to re-enter spawner-adjacent setup safely.
        if let Some(executor) = self.executor.as_ref() {
            self.bind_executor_to_latest_session(executor, session_id);
        }
    }

    fn bind_executor_to_latest_session(
        &self,
        executor: &Arc<dyn SpawnAgentExecutor>,
        mut session_id: String,
    ) {
        loop {
            executor.bind_parent_session(&session_id);
            let Some(latest) = self.current_session_id() else {
                return;
            };
            if latest == session_id {
                return;
            }
            // A concurrent rebind won after this callback started. Reconcile
            // the executor to the authoritative value so callback scheduling
            // cannot leave it attached to an older session.
            session_id = latest;
        }
    }

    fn current_session_id(&self) -> Option<String> {
        self.session_id
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
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

    fn merge_execution_metadata(
        metadata: &mut serde_json::Value,
        execution_metadata: Option<&serde_json::Value>,
    ) {
        let Some(metadata) = metadata.as_object_mut() else {
            return;
        };
        let Some(execution_metadata) = execution_metadata.and_then(serde_json::Value::as_object)
        else {
            return;
        };
        for key in ["workspace", "executor", "transport"] {
            if let Some(value) = execution_metadata.get(key).cloned() {
                metadata.entry(key.to_string()).or_insert(value);
            }
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
        let mut metadata = serde_json::json!({
            "agent_type": &state.agent_type,
            "description": &state.description,
            "status": "spawned",
            "spawn_tool_call_id": &state.spawn_tool_call_id,
            "run_in_background": state.run_in_background,
            "fanout_slot": state.fanout_slot.as_ref().map(|slot| serde_json::json!({
                "group_id": &slot.group_id,
                "target_count": slot.target_count,
                "slot_index": slot.slot_index,
                "slot_id": &slot.slot_id,
            })),
        });
        Self::merge_execution_metadata(&mut metadata, state.execution_metadata.as_ref());
        event.metadata = metadata;
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
        let mut metadata = serde_json::json!({
            "status": status,
            "finish_reason": finish_reason,
            "prompt_tokens": state.metrics.prompt_tokens,
            "completion_tokens": state.metrics.completion_tokens,
            "tool_calls": state.metrics.tool_calls,
            "result_preview": output.map(|text| text.chars().take(500).collect::<String>()),
            "error": error.map(|text| text.chars().take(500).collect::<String>()),
        });
        Self::merge_execution_metadata(&mut metadata, state.execution_metadata.as_ref());
        event.metadata = metadata;
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

    /// Deliver user guidance to one active agent through its runtime mailbox.
    /// The caller supplies the stable message id so UI delivery evidence can
    /// correlate the queued draft with the child's later `received` event.
    pub async fn guide_agent(
        &self,
        agent_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<(), String> {
        if message_id.trim().is_empty() {
            return Err("guidance message identity is missing".into());
        }
        let content = content.trim();
        if content.is_empty() {
            return Err("guidance cannot be empty".into());
        }
        let (parent_run_id, parent_agent_id, to) = {
            let active = self.active_agents.read().await;
            let state = active
                .get(agent_id)
                .ok_or_else(|| "the local runtime no longer owns this active agent".to_string())?;
            let to = state
                .messaging_address
                .clone()
                .ok_or_else(|| "this agent has no active mailbox".to_string())?;
            (
                state.parent_run_id.clone(),
                state.parent_agent_id.clone(),
                to,
            )
        };
        let from = self
            .mailbox_router
            .registered_address_for_agent(&parent_agent_id)
            .await
            .unwrap_or_else(|| AgentAddress::new(parent_run_id, parent_agent_id));
        let mut message = astra_messaging::AgentMessage::new(
            from,
            astra_messaging::MessageTarget::Direct { address: to },
            astra_messaging::MessagePayload::Text {
                content: content.to_string(),
                summary: Some("User guidance".into()),
            },
        )
        .with_ack_required();
        message.id = message_id.to_string();
        self.mailbox_router
            .send(message)
            .await
            .map_err(|error| error.to_string())
    }

    /// Spawn a new agent from the given specification.
    ///
    /// This is called by the `agent(action='spawn')` handler.
    pub async fn spawn(
        &self,
        input: SpawnAgentInput,
        context: &SpawnContext,
    ) -> Result<SpawnAgentOutput, SpawnError> {
        // Keep admission open through all pre-execution side effects and the
        // final JoinSet registration. Shutdown takes the matching write
        // permit before draining, so a concurrently prepared child is either
        // fully supervised or rejected before it mutates lifecycle state.
        let task_admission = self.background_task_admission.read().await;
        if !*task_admission {
            return Err(SpawnError::LifecycleShuttingDown);
        }
        let fanout_slot = input
            .fanout_slot_identity()
            .map_err(SpawnError::InvalidInput)?;
        if context.parent_is_fork_child && input.inherit_prefix.is_some() {
            self.record_fanout_spawn_rejected_for_input(
                fanout_slot.as_ref(),
                &input,
                context,
                "nested fork inheritance is rejected",
            )
            .await;
            return Err(SpawnError::NestedForkInheritanceRejected);
        }

        // Enforce fanout boundary: once a parent run uses a fixed-size
        // fanout group, bare spawns in that run are replacement/retry
        // attempts that bypass the group contract. A later user turn has
        // a new parent_run_id and is unaffected.
        if fanout_slot.is_none() {
            if let Some(group) = self
                .fanout_group_for_parent_run(&context.parent_run_id)
                .await
            {
                return Err(SpawnError::InvalidInput(format!(
                    "parent run '{}' already used agent_fanout group '{}' with fixed target_count {} (status: {}). Do not call agent(action='spawn') to add, retry, or replace agents in the same turn. Present the fanout results, use agent_fanout(action='get_results', group_id='{}') if needed, or ask the user before starting another fanout.",
                    context.parent_run_id,
                    group.group_id,
                    group.target_count,
                    group.status.as_str(),
                    group.group_id
                )));
            }
        }

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
            .unwrap_or_else(|| format!("{}_{}", input.agent_type, Uuid::new_v4()));
        let run_id = Uuid::new_v4().to_string();
        let agent_id = format!("{}@{}", agent_name, run_id);

        // 3. Determine model and turns
        let model = input
            .model
            .clone()
            .or_else(|| agent_def.default_model.clone());
        // Budget resolution composes numeric and complexity ceilings by
        // taking the smaller value; with only one constraint, that constraint
        // is authoritative. See `resolve_turn_budget`.
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
        let Some(executor) = self.executor.as_ref().cloned() else {
            // Executor absence is a host capability failure: no child reached the
            // spawn boundary, so do not materialize a fanout slot that never ran.
            return Err(SpawnError::ExecutorUnavailable);
        };

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
            work_revision: 1,
            messaging_address: None,
            worktree_path: None,
            started_at: SystemTime::now(),
            ended_at: None,
            metrics: Default::default(),
            permission_summary,
            parent_agent_id: context.parent_agent_id.clone(),
            trace_context: context.trace_context.clone(),
            spawn_tool_call_id: context.spawn_tool_call_id.clone(),
            run_in_background: input.run_in_background,
            fanout_slot: fanout_slot.clone(),
            execution_metadata: context.execution_metadata.clone(),
        };
        {
            // Hold the cancellation read fence through reservation. Therefore
            // cancellation either snapshots this child or wins first and
            // rejects it; no descendant can appear after the snapshot.
            let cancellation_fence = self.cancelling_parent_runs.read().await;
            if cancellation_fence.contains(&context.parent_run_id) {
                drop(cancellation_fence);
                self.record_fanout_spawn_rejected_for_input(
                    fanout_slot.as_ref(),
                    &input,
                    context,
                    format!(
                        "parent run '{}' is cancelled; descendant spawn rejected",
                        context.parent_run_id
                    ),
                )
                .await;
                return Err(SpawnError::Race(format!(
                    "parent run '{}' is cancelled; descendant spawn rejected",
                    context.parent_run_id
                )));
            }
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
                                &context.parent_run_id,
                            )
                            .await;
                    }
                    return Err(SpawnError::ConcurrencyLimitExceeded { active, limit });
                }
            }
            active_agents.insert(agent_id.clone(), state);
            drop(active_agents);
            drop(cancellation_fence);
        }

        // 5. Every dynamic agent is an addressable runtime object. Mailbox
        // capability follows the generated run/agent identity, never the
        // optional user-facing `name` field.
        let addr = AgentAddress::new(&run_id, &agent_id);
        let delegation_id = Some(context.parent_run_id.clone());
        let mailbox = match self
            .mailbox_router
            .register(addr.clone(), delegation_id)
            .await
        {
            Ok(mailbox) => Some(mailbox),
            Err(error) => {
                self.active_agents.write().await.remove(&agent_id);
                self.record_fanout_spawn_rejected_for_input(
                    fanout_slot.as_ref(),
                    &input,
                    context,
                    format!("mailbox registration failed: {error}"),
                )
                .await;
                return Err(SpawnError::MailboxRegistration(error.to_string()));
            }
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
                    cleanup_agent_worktree(worktree_path.as_ref(), &agent_id);
                    self.record_fanout_spawn_rejected_for_input(
                        fanout_slot.as_ref(),
                        &input,
                        context,
                        format!("agent {agent_id} was cancelled before spawn completed"),
                    )
                    .await;
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
                    &run_id,
                    &input.agent_type,
                    &input.description,
                    context.spawn_tool_call_id.as_deref(),
                    &context.parent_run_id,
                )
                .await
        {
            self.active_agents.write().await.remove(&agent_id);
            if let Some(addr) = messaging_address.as_ref() {
                let _ = self.mailbox_router.unregister(addr).await;
            }
            cleanup_agent_worktree(worktree_path.as_ref(), &agent_id);
            return Err(error);
        }
        self.emit_agent_spawned_trace(&spawned_state_for_trace)
            .await;
        self.publish_background_agent(&spawned_state_for_trace);

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
        let emitter = self.progress_broadcaster.for_agent_with_run_context(
            agent_id.clone(),
            run_id.clone(),
            context.parent_run_id.clone(),
            context.execution_metadata.clone(),
        );
        emitter.started(&input.description);
        emitter.agent_spawned_with_fanout(
            &input.agent_type,
            &input.description,
            fanout_slot.clone(),
        );

        // 7. Build parent address for permission requests
        let parent_address = astra_messaging::types::AgentAddress::new(
            &context.parent_run_id,
            &context.parent_agent_id,
        );

        // 7b. Build permission context from explicit inherited permissions.
        // SpawnContext requires an envelope so a child cannot enter runtime
        // execution without an authorization context.
        //
        // The agent type's `allowed_tools` is merged into
        // `inherited_permissions` here so the permission engine's
        // `ToolAllowlist` evaluation step enforces it as the single
        // source of truth (review C1-arch). Previously
        // `run_config.allowed_tools` carried the list for prompt
        // pruning but the engine skipped the allowlist step when
        // `inherited.allowed_tools` was `None` — letting spawned
        // agents call tools outside their declared surface.
        let inherited_permissions = context
            .inherited_permissions
            .clone()
            .with_allowed_tools(agent_def.allowed_tools.iter().cloned());
        let permission_context =
            super::permission_sync::PermissionSyncContext::shared(inherited_permissions.clone());

        // 8. Build run config
        let coordination_addendum = format!(
            "{}\n\n## Parent coordination\n\
             Your run_id is `{run_id}` and agent_id is `{agent_id}`. Your parent is agent `{}` on run `{}`. \
             Stay within the delegated task boundary. Use `agent(action=\"send_message\", to=\"parent\", ...)` only when you are blocked, need a decision, discover information that materially changes the parent plan, or have a concise milestone worth acting on. \
             Routine tool-by-tool progress does not need reporting. Your terminal result is delivered to the parent automatically.",
            agent_def.system_prompt_addendum, context.parent_agent_id, context.parent_run_id,
        );
        let run_config = SpawnRunConfig {
            run_id: run_id.clone(),
            agent_id: agent_id.clone(),
            spawn_tool_call_id: context.spawn_tool_call_id.clone(),
            recursion_depth: child_recursion_depth,
            agent_type: input.agent_type.clone(),
            description: input.description.clone(),
            task: input.prompt.clone(),
            system_prompt_addendum: coordination_addendum,
            model,
            max_turns,
            allowed_tools: agent_def.allowed_tools.iter().cloned().collect(),
            read_only: agent_def.read_only,
            working_dir: worktree_path.unwrap_or_else(|| context.working_dir.clone()),
            mailbox,
            progress_emitter: Some(emitter.clone()),
            context_cache: Some(Arc::clone(&self.context_cache)),
            // Inherit permissions from parent context
            inherited_permissions,
            // Parent address for permission requests
            parent_address: Some(parent_address),
            // Permission context for runtime permission management
            permission_context,
            // Skills inherited from parent
            inherited_skills: context.inherited_skills.clone(),
            live_event_sink: context.live_event_sink.clone(),
            client_tool_delivery_tx: context.client_tool_delivery_tx.clone(),
            execution_metadata: context.execution_metadata.clone(),
            is_fork_child: inherited_prefix.is_some(),
            inherited_prefix,
            delegation_chain: context.delegation_chain.clone(),
        };

        // Emit agent_spawned journal event for unified timeline.
        if let Some(sid) = self.current_session_id() {
            let evt = astra_services::session_journal::JournalEvent::agent_spawned(
                Some(&sid),
                &agent_id,
                &run_id,
                &context.parent_run_id,
                &run_config.agent_type,
                &input.description,
                run_config.model.as_deref(),
                run_config.inherited_prefix.is_some(),
                run_config.execution_metadata.as_ref(),
            );
            if let Ok(writer) = astra_services::session_journal::JournalWriter::new(&sid) {
                let _ = writer.append(&evt);
            }
        }

        // 8. Execute or launch. Both explicit background spawns and
        // foreground sync spawns run through the same task/finalization
        // pipe. Sync mode simply waits for the terminal oneshot unless
        // Ctrl+B promotes the wait into a background `Launched` result.
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
        let executor = Arc::clone(&executor);
        let spawner = self.clone_for_task();
        let spawner_for_finalize_repair = spawner.clone_for_task();
        let agent_id_for_task = agent_id.clone();
        let agent_id_for_output = agent_id.clone();
        let agent_id_for_finalize_panic = agent_id.clone();
        let run_id_for_output = run_id.clone();
        let run_id_for_finalize_panic = run_id.clone();
        let spawn_future = async move {
            let result = AssertUnwindSafe(executor.execute(run_config))
                .catch_unwind()
                .await;
            // Phase 2: turn the result into a terminal output by finalizing
            // the agent. Wrap finalization in `catch_unwind` so a panic in
            // `finalize_background_agent` (or the status/output builders)
            // cannot (a) silently drop `terminal_tx` before the front-end
            // observes a terminal state, or (b) leak a zombie entry in the
            // active-agents / completion-notifier bookkeeping across the
            // host task. The oneshot is guaranteed to receive a terminal
            // output. `finalize_background_agent` is idempotent (guards on
            // `active_agents.remove`), so even a partial-mutation panic is
            // observable as a clean `Failed` here rather than a dropped task.
            let finalize = AssertUnwindSafe(async move {
                match result {
                    Ok(Ok(run_result)) => {
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
                        spawn_run_result_to_sync_output(
                            agent_id_for_output,
                            run_id_for_output.clone(),
                            run_result,
                            duration_ms,
                        )
                    }
                    Ok(Err(error)) => {
                        let duration_ms = started_at
                            .elapsed()
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
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
                            agent_id: agent_id_for_output.clone(),
                            run_id: run_id_for_output.clone(),
                            error,
                            finish_reason: "failed".to_string(),
                            duration_ms,
                        }
                    }
                    Err(panic) => {
                        let error = format!(
                            "agent executor panicked: {}",
                            panic_payload_message(panic.as_ref())
                        );
                        let duration_ms = started_at
                            .elapsed()
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        spawner
                            .finalize_background_agent(
                                &agent_id_for_task,
                                AgentStatus::Failed {
                                    error: error.clone(),
                                    finish_reason: Some("panic".to_string()),
                                },
                                "failed",
                                Some("panic"),
                                None,
                                None,
                                Some(error.as_str()),
                            )
                            .await;
                        SpawnAgentOutput::Failed {
                            agent_id: agent_id_for_output.clone(),
                            run_id: run_id_for_output.clone(),
                            error,
                            finish_reason: "panic".to_string(),
                            duration_ms,
                        }
                    }
                }
            });
            let output = match finalize.catch_unwind().await {
                Ok(output) => output,
                Err(panic) => {
                    spawner_for_finalize_repair.repair_fanout_slot_count().await;
                    let duration_ms = started_at
                        .elapsed()
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    SpawnAgentOutput::Failed {
                        agent_id: agent_id_for_finalize_panic.clone(),
                        run_id: run_id_for_finalize_panic.clone(),
                        error: format!(
                            "agent finalization panicked: {}",
                            panic_payload_message(panic.as_ref())
                        ),
                        finish_reason: "panic".to_string(),
                        duration_ms,
                    }
                }
            };
            let _ = terminal_tx.send(output);
        };
        let Some(background_tasks) = self.background_tasks.upgrade() else {
            // The root/session owner disappeared while this method was
            // borrowed through a task-side handle. Converge the reservation
            // through the canonical cancellation path rather than leaving a
            // running projection with no host future.
            drop(task_admission);
            let _ = self
                .cancel_agent_with_origin(
                    &agent_id,
                    "agent lifecycle owner disappeared before execution",
                    CancelOrigin::System,
                )
                .await;
            return Err(SpawnError::LifecycleShuttingDown);
        };
        let abort_handle = background_tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .spawn(spawn_future);
        self.background_abort_handles
            .write()
            .await
            .insert(agent_id.clone(), abort_handle);
        drop(task_admission);

        if input.run_in_background {
            return Ok(SpawnAgentOutput::Launched {
                agent_id,
                run_id,
                description,
                messaging_address: messaging_address_text,
            });
        }

        loop {
            if self.take_foreground_promotion_request(&agent_id).await {
                return Ok(SpawnAgentOutput::Launched {
                    agent_id,
                    run_id,
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
                    return Ok(terminal.unwrap_or_else(|_| {
                        let duration_ms = started_at
                            .elapsed()
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        dropped_agent_terminal_output(&agent_id, &run_id, duration_ms)
                    }));
                }
                _ = notify.notified() => {}
            }
        }
    }

    /// Cancel a single background agent by id. Returns true only when this call
    /// actually owned the cancellation and archived the agent as cancelled.
    pub async fn cancel_agent(&self, agent_id: &str, reason: &str) -> bool {
        self.cancel_agent_with_origin(agent_id, reason, CancelOrigin::User)
            .await
    }

    /// Cancel every live dynamic-agent descendant of `parent_run_id`.
    ///
    /// Dynamic fanout tasks are owned by the session spawner rather than the
    /// parent loop's `JoinHandle`, so dropping/cancelling the parent future is
    /// not sufficient. Snapshot the run tree first, then cancel deepest-first
    /// without holding an agent-map lock across persistence or mailbox I/O.
    pub(crate) async fn cancel_descendants_of_parent_run(
        &self,
        parent_run_id: &str,
        reason: DescendantCancellationReason,
    ) -> usize {
        let reason = reason.as_str();
        // Serialize the snapshot boundary with spawn reservation. Any spawn
        // already holding the read fence finishes insertion and is included;
        // any later spawn observes the cancellation marker and is rejected.
        let mut cancellation_fence = self.cancelling_parent_runs.write().await;
        cancellation_fence.insert(parent_run_id.to_string());
        let mut children_by_parent: HashMap<String, Vec<(String, String)>> = HashMap::new();
        {
            let active = self.active_agents.read().await;
            for state in active.values() {
                children_by_parent
                    .entry(state.parent_run_id.clone())
                    .or_default()
                    .push((state.agent_id.clone(), state.run_id.clone()));
            }
        }

        let mut pending = VecDeque::from([parent_run_id.to_string()]);
        let mut visited_runs = HashSet::new();
        let mut descendants = Vec::new();
        while let Some(run_id) = pending.pop_front() {
            if !visited_runs.insert(run_id.clone()) {
                continue;
            }
            let Some(children) = children_by_parent.get(&run_id) else {
                continue;
            };
            for (agent_id, child_run_id) in children {
                descendants.push(agent_id.clone());
                cancellation_fence.insert(child_run_id.clone());
                pending.push_back(child_run_id.clone());
            }
        }
        drop(cancellation_fence);

        let mut cancelled = 0;
        for agent_id in descendants.into_iter().rev() {
            if self
                .cancel_agent_with_origin(&agent_id, reason, CancelOrigin::System)
                .await
            {
                cancelled += 1;
            }
        }
        cancelled
    }

    async fn cancel_agent_with_origin(
        &self,
        agent_id: &str,
        reason: &str,
        origin: CancelOrigin,
    ) -> bool {
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

        let durable_cancel = if let Some(executor) = self.executor.as_ref() {
            executor
                .cancel_spawned_run(
                    &state.run_id,
                    state
                        .trace_context
                        .as_ref()
                        .map(|trace| trace.user_id.as_str()),
                    reason,
                )
                .await
        } else {
            Ok(())
        };
        abort_handle.abort();
        if let Err(error) = durable_cancel {
            astra_core::agent_warn!(
                "spawner",
                "executor cancellation for {agent_id} failed: {error}"
            );
        }
        // Public `cancel_agent` is user-driven (Ctrl+G x, /agent cancel,
        // etc.); bounded one-shot shutdown uses the same atomic finalization
        // without mislabeling the deadline as user intent.
        self.finalize_cancelled_agent(&mut state, agent_id, reason, origin)
            .await
    }

    /// Finalize an agent atomically seized by the cancellation path.
    /// Performs all the same cleanup as [`finalize_background_agent`] but
    /// operates on a pre-extracted [`SpawnedAgentState`] — the caller already
    /// owns the state and the abort handle is already removed from the book.
    async fn finalize_cancelled_agent(
        &self,
        state: &mut SpawnedAgentState,
        agent_id: &str,
        reason: &str,
        origin: CancelOrigin,
    ) -> bool {
        self.foreground_promotion_requests
            .write()
            .await
            .remove(agent_id);

        let status = match origin {
            CancelOrigin::User => AgentStatus::cancelled_by_user(reason),
            CancelOrigin::System => AgentStatus::Cancelled {
                by_user: false,
                reason: reason.to_string(),
            },
        };
        state.status = status;
        state.work_revision = state.work_revision.saturating_add(1);
        state.ended_at = Some(SystemTime::now());
        let messaging_address = state.messaging_address.take();

        self.record_fanout_terminal_state(state).await;
        self.publish_background_agent(state);
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
                run_id: state.run_id.clone(),
                parent_run_id: state.parent_run_id.clone(),
                event_type,
                timestamp_epoch_ms,
                metadata: state.execution_metadata.clone(),
            });
        }
        if let Some(addr) = messaging_address {
            self.deliver_terminal_result_to_parent(state, &addr).await;
            if let Err(err) = self.mailbox_router.unregister(&addr).await {
                eprintln!(
                    "  ⚠ messaging: failed to unregister mailbox for '{}': {}",
                    agent_id, err
                );
            }
        }
        let worktree_path = state.worktree_path.take();
        self.archive_state(state.clone()).await;
        self.cleanup_worktree(worktree_path, agent_id).await;
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
        self.foreground_promotion_requests
            .write()
            .await
            .remove(agent_id);
        self.background_abort_handles.write().await.remove(agent_id);
        let (mut state, messaging_address) = {
            let mut active_agents = self.active_agents.write().await;
            let Some(mut state) = active_agents.remove(agent_id) else {
                return false;
            };
            if let Some(run_result) = run_result {
                state.metrics.turns_completed = run_result.turns_completed;
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
            state.work_revision = state.work_revision.saturating_add(1);
            state.ended_at = Some(SystemTime::now());
            let messaging_address = state.messaging_address.take();
            (state, messaging_address)
        };

        self.record_fanout_terminal_state(&state).await;
        self.publish_background_agent(&state);
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
                run_id: state.run_id.clone(),
                parent_run_id: state.parent_run_id.clone(),
                event_type,
                timestamp_epoch_ms,
                metadata: state.execution_metadata.clone(),
            });
        }
        if let Some(addr) = messaging_address {
            self.deliver_terminal_result_to_parent(&state, &addr).await;
            if let Err(err) = self.mailbox_router.unregister(&addr).await {
                eprintln!(
                    "  ⚠ messaging: failed to unregister mailbox for '{}': {}",
                    agent_id, err
                );
            }
        }
        let worktree_path = state.worktree_path.take();
        self.archive_state(state).await;
        self.cleanup_worktree(worktree_path, agent_id).await;
        self.notify_completion(agent_id).await;
        true
    }

    async fn deliver_terminal_result_to_parent(
        &self,
        state: &SpawnedAgentState,
        from: &AgentAddress,
    ) {
        if !state.run_in_background {
            return;
        }
        let payload = match &state.status {
            AgentStatus::Completed { result, .. } => {
                MessagePayload::Signal(astra_messaging::AgentSignal::Completed {
                    output: result.clone(),
                })
            }
            AgentStatus::Failed { error, .. } => {
                MessagePayload::Signal(astra_messaging::AgentSignal::Failed {
                    error: error.clone(),
                })
            }
            AgentStatus::Interrupted {
                partial_result,
                finish_reason,
            } => MessagePayload::Signal(astra_messaging::AgentSignal::Failed {
                error: format!("{finish_reason}: {partial_result}"),
            }),
            AgentStatus::Cancelled { reason, .. } => {
                MessagePayload::Signal(astra_messaging::AgentSignal::Failed {
                    error: format!("cancelled: {reason}"),
                })
            }
            AgentStatus::Waiting { reason } => {
                MessagePayload::Signal(astra_messaging::AgentSignal::Waiting {
                    reason: reason.clone(),
                })
            }
            AgentStatus::Initializing | AgentStatus::Running { .. } | AgentStatus::Idle => return,
        };
        let message = AgentMessage::new(from.clone(), MessageTarget::Parent, payload);
        if let Err(error) = self.mailbox_router.send(message).await {
            tracing::warn!(
                target: "astra_runtime::messaging",
                agent_id = %state.agent_id,
                run_id = %state.run_id,
                parent_run_id = %state.parent_run_id,
                error = %error,
                "failed to deliver terminal child result to parent mailbox"
            );
        }
    }

    /// Persist final agent state to session journal (best-effort).
    async fn persist_agent_terminated_state(
        &self,
        state: &SpawnedAgentState,
        status: &str,
        finish_reason: Option<&str>,
    ) {
        let Some(sid) = self.current_session_id() else {
            return;
        };
        let writer = match astra_services::session_journal::JournalWriter::new(&sid) {
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
            (state.metrics.turns_completed > 0).then_some(state.metrics.turns_completed),
            state.metrics.tool_calls,
            state.metrics.prompt_tokens,
            state.metrics.completion_tokens,
            duration_ms,
            state.execution_metadata.as_ref(),
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
            completed.pop_front();
        }
        completed.push_back(state);
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
            AgentStatus::Waiting { .. } => "waiting",
            AgentStatus::Completed { .. } => "completed",
            AgentStatus::Interrupted { .. } => "interrupted",
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
        let mut metadata = serde_json::json!({
            "child_agent_id": agent_id,
            "child_run_id": &state.run_id,
            "child_status": Self::agent_status_trace_label(child_status),
        });
        Self::merge_execution_metadata(&mut metadata, state.execution_metadata.as_ref());
        event.metadata = metadata;
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

    /// Clone a non-owning spawner handle for use inside supervised tasks.
    ///
    /// The returned handle shares lifecycle state and can register nested
    /// children while the root owner is alive, but it cannot keep the
    /// supervisor alive. This is the ownership boundary that makes an
    /// ordinary root drop cancel its task tree instead of creating a
    /// `JoinSet -> future -> spawner -> JoinSet` reference cycle.
    fn clone_for_task(&self) -> Self {
        Self {
            mailbox_router: Arc::clone(&self.mailbox_router),
            active_agents: Arc::clone(&self.active_agents),
            cancelling_parent_runs: Arc::clone(&self.cancelling_parent_runs),
            progress_broadcaster: Arc::clone(&self.progress_broadcaster),
            context_cache: Arc::clone(&self.context_cache),
            executor: self.executor.clone(),
            session_id: self.session_id.clone(),
            agent_registry: self.agent_registry.clone(),
            completed_agents: Arc::clone(&self.completed_agents),
            _background_task_owner: None,
            background_tasks: self.background_tasks.clone(),
            background_task_admission: Arc::clone(&self.background_task_admission),
            background_abort_handles: Arc::clone(&self.background_abort_handles),
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
            fanout_agent_index: Arc::clone(&self.fanout_agent_index),
            fanout_terminal_result_cache: Arc::clone(&self.fanout_terminal_result_cache),
            cached_active_fanout_slots: Arc::clone(&self.cached_active_fanout_slots),
            durable_reconciler: Arc::clone(&self.durable_reconciler),
            durable_observed_agent_ids: Arc::clone(&self.durable_observed_agent_ids),
            durable_reconcile_lock: Arc::clone(&self.durable_reconcile_lock),
            active_work_registry: self.active_work_registry.clone(),
        }
    }

    /// Non-owning lifecycle handle for an executor running inside this
    /// spawner's own task tree. Server sub-runs use this when wiring nested
    /// agent tools so the child executor cannot retain its supervisor.
    pub(crate) fn task_handle(&self) -> Arc<Self> {
        Arc::new(self.clone_for_task())
    }

    /// Drain all background agents, wait up to `deadline`, and return
    /// terminal results so the caller can surface them.
    ///
    /// Returns `(agent_id, result_text)` for every terminal background child.
    /// Failed, interrupted, cancelled, and deadline-exceeded runs are rendered
    /// explicitly instead of disappearing from the one-shot caller's output.
    /// Tasks that exceed `deadline` are cancelled through the normal
    /// finalization path before their host tasks are reaped.
    pub async fn shutdown_and_wait(&self, deadline: std::time::Duration) -> Vec<(String, String)> {
        self.shutdown_and_wait_with_reason(
            deadline,
            "one-shot caller deadline elapsed while waiting for background agent",
        )
        .await
    }

    /// Drain background agents within `deadline`, then cancel unfinished work
    /// as a system lifecycle action using the caller-provided reason.
    ///
    /// Session shutdown and session rebind are not user cancellation. Keeping
    /// that distinction here prevents frontends from calling the public
    /// user-action API merely to converge owned work during teardown.
    pub async fn shutdown_and_wait_with_reason(
        &self,
        deadline: std::time::Duration,
        reason: &str,
    ) -> Vec<(String, String)> {
        // Close admission before taking the JoinSet. The write permit waits
        // for any spawn currently preparing side effects to finish
        // registration, eliminating the old race where a nested child could
        // be inserted into a fresh, unobserved JoinSet during shutdown.
        let mut admission = self.background_task_admission.write().await;
        *admission = false;
        let mut set = self.background_tasks.upgrade().map(|tasks| {
            tasks
                .lock()
                .map(|mut guard| std::mem::take(&mut *guard))
                .unwrap_or_else(|poisoned| {
                    let mut guard = poisoned.into_inner();
                    std::mem::take(&mut *guard)
                })
        });
        drop(admission);

        // Drain JoinSet — even if empty (tasks may have already completed).
        match tokio::time::timeout(deadline, async {
            let Some(set) = set.as_mut() else {
                return;
            };
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
                    "background agent drain timed out after {deadline:?}; cancelling remaining tasks"
                );

                // Do not abort the JoinSet first: dropping an agent future
                // bypasses its terminal finalizer and leaves both a zombie
                // active-agent projection and no user-visible explanation.
                // `cancel_agent` atomically owns and archives each live child,
                // emits its terminal event, and then aborts the host task.
                // `active_agents` is the canonical live-work set. The old
                // background-only ID ledger both raced fast completions and
                // omitted foreground children still waiting inside a tool
                // call, causing abort to bypass their terminal finalizer.
                let unfinished = self
                    .active_agents
                    .read()
                    .await
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
                for agent_id in unfinished {
                    let _ = self
                        .cancel_agent_with_origin(&agent_id, reason, CancelOrigin::System)
                        .await;
                }
                if let Some(set) = set.as_mut() {
                    set.abort_all();
                    while set.join_next().await.is_some() {}
                }
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
                let result = match &s.status {
                    AgentStatus::Completed { result, .. } => result.clone(),
                    AgentStatus::Failed { error, .. } => format!("Agent failed: {error}"),
                    AgentStatus::Interrupted {
                        partial_result,
                        finish_reason,
                    } => format!("Agent interrupted ({finish_reason}): {partial_result}"),
                    AgentStatus::Cancelled { reason, .. } => {
                        format!("Agent cancelled: {reason}")
                    }
                    AgentStatus::Waiting { reason } => {
                        format!("Agent needs input or cannot continue: {reason}")
                    }
                    AgentStatus::Initializing | AgentStatus::Running { .. } | AgentStatus::Idle => {
                        return None;
                    }
                };
                Some((s.agent_id.clone(), result))
            })
            .collect()
    }

    /// Number of in-flight background tasks currently tracked.
    /// Primarily useful for tests and observability.
    pub fn background_task_count(&self) -> usize {
        self.background_tasks
            .upgrade()
            .map(|tasks| {
                tasks
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .len()
            })
            .unwrap_or(0)
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

    /// Return full spawned-agent states that belong to a root run's dynamic-agent tree.
    ///
    /// Unlike [`get_agent_history`], this keeps execution metadata so server/Web
    /// stream repair can rebuild lifecycle events without losing workspace or
    /// executor binding details.
    pub(crate) async fn get_agent_states_for_run_tree(
        &self,
        root_run_id: &str,
    ) -> Vec<SpawnedAgentState> {
        let mut states: Vec<SpawnedAgentState> =
            self.completed_agents.read().await.iter().cloned().collect();
        let active_states = self.active_agents.read().await;
        for state in active_states.values() {
            if !states
                .iter()
                .any(|existing| existing.agent_id == state.agent_id)
            {
                states.push(state.clone());
            }
        }
        drop(active_states);

        let mut run_ids = HashSet::from([root_run_id.to_string()]);
        let mut emitted_agent_ids = HashSet::new();
        let mut tree = Vec::new();
        loop {
            let mut advanced = false;
            for state in &states {
                if emitted_agent_ids.contains(&state.agent_id) {
                    continue;
                }
                if run_ids.contains(&state.parent_run_id) {
                    run_ids.insert(state.run_id.clone());
                    emitted_agent_ids.insert(state.agent_id.clone());
                    tree.push(state.clone());
                    advanced = true;
                }
            }
            if !advanced {
                break;
            }
        }
        tree.sort_by(|left, right| {
            left.started_at
                .cmp(&right.started_at)
                .then_with(|| left.run_id.cmp(&right.run_id))
                .then_with(|| left.agent_id.cmp(&right.agent_id))
        });
        tree
    }

    /// Update agent status.
    pub async fn update_status(&self, agent_id: &str, status: AgentStatus) {
        if let Some(state) = self.active_agents.write().await.get_mut(agent_id) {
            if state.status == status {
                return;
            }
            state.status = status.clone();
            state.work_revision = state.work_revision.saturating_add(1);
            if status.is_terminal() {
                state.ended_at.get_or_insert_with(SystemTime::now);
            }
            self.publish_background_agent(state);

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
                run_id: state.run_id.clone(),
                parent_run_id: state.parent_run_id.clone(),
                event_type,
                timestamp_epoch_ms,
                metadata: state.execution_metadata.clone(),
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

    #[error("Agent executor unavailable: spawned agents cannot run in this context")]
    ExecutorUnavailable,

    /// The session owner has closed spawn admission and is draining its task
    /// tree. This is a lifecycle boundary, not a retryable model/provider
    /// failure; callers must start work in the replacement session runtime.
    #[error("Agent lifecycle is shutting down; no new child work is accepted")]
    LifecycleShuttingDown,

    /// Fork children are allowed to spawn normal children, but not
    /// another inherit-prefix fork. This mirrors the reference agent's
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

    /// Fired when `MAX_FANOUT_GROUPS` is reached and no terminal group is
    /// available for LRU eviction (every group is still live). Distinct from
    /// `ConcurrencyLimitExceeded` which caps *running* agents — this caps the
    /// *bookkeeping map* so a long session can't leak memory through
    /// abandoned-but-not-settled group metadata.
    #[error(
        "Fanout group limit reached: {active} groups (limit {limit}), all live. \
         Wait for an existing group to settle or cancel its agents before spawning more."
    )]
    FanoutGroupLimitExceeded { active: usize, limit: usize },

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

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "unknown panic payload".to_string()
}

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

    let inherited = &context.inherited_permissions;
    summary.mode = match inherited.mode {
        super::permission_sync::PermissionMode::Auto => "auto".to_string(),
        super::permission_sync::PermissionMode::Bypass => "bypass".to_string(),
        super::permission_sync::PermissionMode::Plan => "plan".to_string(),
        super::permission_sync::PermissionMode::AcceptEdits => "accept_edits".to_string(),
        super::permission_sync::PermissionMode::Prompt => "prompt".to_string(),
        super::permission_sync::PermissionMode::Deny => "deny".to_string(),
    };
    summary.allow_rules = inherited.allow_rules.len() as u32;
    summary.deny_rules = inherited.deny_rules.len() as u32;
    // Has parent if parent_run_id is not empty and not "root"
    summary.has_parent = !context.parent_run_id.is_empty() && context.parent_run_id != ROOT_RUN_ID;

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

fn cleanup_agent_worktree(worktree_path: Option<&PathBuf>, agent_id: &str) {
    let Some(path) = worktree_path else {
        return;
    };
    match remove_git_agent_worktree(path) {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            astra_core::agent_debug!(
                "spawner",
                "git worktree cleanup probe failed for {agent_id} at {}: {error}",
                path.display()
            );
        }
    }
    if !path.exists() {
        return;
    }
    if let Err(error) = std::fs::remove_dir_all(path) {
        astra_core::agent_warn!(
            "spawner",
            "failed to clean up worktree for {agent_id} at {}: {error}",
            path.display()
        );
    }
}

fn remove_git_agent_worktree(path: &Path) -> Result<bool, std::io::Error> {
    let Some(worktree_base) = path.parent() else {
        return Ok(false);
    };
    let Some(parent_dir) = worktree_base.parent() else {
        return Ok(false);
    };
    let output = std::process::Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(path)
        .current_dir(parent_dir)
        .output()?;
    Ok(output.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::delegation::engine::DelegationTracker;
    use astra_messaging::in_process::InProcessTransport;
    use astra_messaging::router::AgentMailboxRouter;
    use astra_messaging::types::{AgentMessage, MessagePayload, MessageTarget};
    use serde_json::json;
    use tokio::time::{Duration, sleep};

    fn mock_router() -> Arc<AgentMailboxRouter> {
        let transport = Arc::new(InProcessTransport::new());
        let dt = Arc::new(DelegationTracker::new());
        Arc::new(AgentMailboxRouter::new(transport, dt))
    }

    fn durable_run(
        run_id: &str,
        depth: u32,
        status: &str,
    ) -> astra_services::runs::DurableRunRecord {
        astra_services::runs::DurableRunRecord {
            run_id: run_id.into(),
            user_id: "user-1".into(),
            session_id: "session-1".into(),
            parent_run_id: (depth > 0).then(|| "root-run".into()),
            root_run_id: Some("root-run".into()),
            ancestor_path: Some(if depth == 0 {
                "root-run".into()
            } else {
                format!("root-run/{run_id}")
            }),
            depth,
            delegation_id: None,
            agent_id: None,
            retry_of: None,
            retry_scope: None,
            status: status.into(),
            waiting_for: None,
            owner_pod_id: None,
            owner_lease_expires_at: None,
            run_generation: 0,
            last_event_idx: -1,
            checkpoint_version: None,
            checkpoint_json: None,
            error_code: None,
            error_message: None,
            retry_count: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tool_calls: 0,
            agent_binding_id: None,
            agent_binding_name: None,
            agent_binding_schema_version: None,
            selected_model_json: None,
            selected_model_name: None,
            selected_model_gateway: None,
            capability_server_refs_json: None,
            runtime_profile: None,
            events: Vec::new(),
            created_at: "2026-07-13T00:00:00Z".into(),
            updated_at: "2026-07-13T00:00:00Z".into(),
        }
    }

    #[test]
    fn durable_partial_child_restores_as_interrupted_with_exact_reason() {
        let mut run = durable_run("partial-child", 1, astra_core::STATUS_FAILED);
        run.error_code =
            Some(astra_services::coordination::AGENT_RESULT_PARTIAL_DURABLE_ERROR_CODE.to_string());
        run.error_message = Some("budget_exhausted: adaptive hard turn limit reached".to_string());
        run.events.push(json!({
            "event_type": "text_done",
            "data": {"full_text": "Partial architecture findings."}
        }));

        assert!(matches!(
            durable_agent_status(&run),
            AgentStatus::Interrupted {
                partial_result,
                finish_reason,
            } if partial_result == "Partial architecture findings."
                && finish_reason == "budget_exhausted: adaptive hard turn limit reached"
        ));
    }

    struct StaticDurableReconciler {
        runs: Vec<astra_services::runs::DurableRunRecord>,
    }

    #[async_trait]
    impl DurableAgentReconciler for StaticDurableReconciler {
        async fn load_agent_recovery(
            &self,
        ) -> Result<Vec<astra_services::runs::DurableRunRecord>, String> {
            Ok(self.runs.clone())
        }
    }

    #[tokio::test]
    async fn durable_restore_rebuilds_completed_fanout_and_is_idempotent() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let mut root = durable_run("root-run", 0, astra_core::STATUS_COMPLETED);
        root.events.push(json!({
            "type": "agent_spawned",
            "run_id": "child-run",
            "agent_id": "reviewer-1",
            "agent_type": "code-review",
            "description": "Review correctness",
            "fanout_slot": {
                "group_id": "review-group",
                "target_count": 1,
                "slot_index": 0,
                "slot_id": "correctness"
            }
        }));
        let mut child = durable_run("child-run", 1, astra_core::STATUS_COMPLETED);
        child.agent_id = Some("reviewer-1".into());
        child.total_tool_calls = 7;
        child.events.push(json!({
            "event_type": "text_done",
            "data": {"full_text": "No correctness issues found."}
        }));

        assert_eq!(
            spawner
                .restore_durable_agent_runs(&[root.clone(), child.clone()])
                .await,
            1
        );
        assert_eq!(
            spawner.restore_durable_agent_runs(&[root, child]).await,
            0,
            "recovery must not duplicate an already reconstructed child"
        );

        let state = spawner
            .get_agent_state_any("reviewer-1")
            .await
            .expect("restored child");
        assert_eq!(state.run_id, "child-run");
        assert_eq!(state.metrics.tool_calls, 7);
        assert!(matches!(
            state.status,
            AgentStatus::Completed { ref result, .. }
                if result == "No correctness issues found."
        ));

        let group = spawner
            .fanout_group_for_agent("reviewer-1")
            .await
            .expect("restored fanout group");
        assert_eq!(group.status, AgentFanoutStatus::Finished);
        assert_eq!(group.slots[0].run_id.as_deref(), Some("child-run"));
        assert_eq!(group.slots[0].status, AgentFanoutSlotStatus::Completed);
    }

    #[tokio::test]
    async fn durable_reconciliation_converges_remote_waiting_child_to_completion() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let mut root = durable_run("root-run", 0, astra_core::STATUS_RUNNING);
        root.events.push(json!({
            "type": "agent_spawned",
            "run_id": "child-run",
            "agent_id": "reviewer-1",
            "agent_type": "code-review",
            "description": "Review correctness",
            "fanout_slot": {
                "group_id": "review-group",
                "target_count": 1,
                "slot_index": 0,
                "slot_id": "correctness"
            }
        }));
        let mut running = durable_run("child-run", 1, astra_core::STATUS_RUNNING);
        running.agent_id = Some("reviewer-1".into());
        running.owner_pod_id = Some("other-pod".into());
        assert_eq!(
            spawner
                .restore_durable_agent_runs(&[root, running.clone()])
                .await,
            1
        );
        let group = spawner
            .fanout_group_for_agent("reviewer-1")
            .await
            .expect("running fanout group");
        assert_eq!(group.status, AgentFanoutStatus::Running);
        assert_eq!(group.slots[0].status, AgentFanoutSlotStatus::Running);

        let mut completed = running;
        completed.status = astra_core::STATUS_COMPLETED.into();
        completed.total_tool_calls = 9;
        completed.events.push(json!({
            "event_type": "text_done",
            "data": {"full_text": "Remote child completed."}
        }));
        spawner
            .set_durable_agent_reconciler(Arc::new(StaticDurableReconciler {
                runs: vec![completed],
            }))
            .await;

        assert_eq!(spawner.reconcile_durable_agent_runs().await.unwrap(), 1);
        let state = spawner
            .get_agent_state_any("reviewer-1")
            .await
            .expect("reconciled child");
        assert!(matches!(
            state.status,
            AgentStatus::Completed { ref result, .. } if result == "Remote child completed."
        ));
        assert_eq!(state.metrics.tool_calls, 9);
        let group = spawner
            .fanout_group_for_agent("reviewer-1")
            .await
            .expect("completed fanout group");
        assert_eq!(group.status, AgentFanoutStatus::Finished);
        assert_eq!(group.slots[0].status, AgentFanoutSlotStatus::Completed);
    }

    #[tokio::test]
    async fn test_spawn_basic() {
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>);
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
            inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
            inherited_skills: vec![],
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            spawn_tool_call_id: None,
            execution_metadata: None,
            delegation_chain: Vec::new(),
        };

        let result = spawner.spawn(input, &context).await.unwrap();
        assert!(matches!(result, SpawnAgentOutput::Launched { .. }));
    }

    #[tokio::test]
    async fn spawn_builds_permission_context_from_explicit_inherited_permissions() {
        let executor = Arc::new(CapturingPermissionExecutor::new());
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(executor.clone() as Arc<dyn SpawnAgentExecutor>);
        let input = SpawnAgentInput {
            description: "Test agent".to_string(),
            prompt: "Do a test".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: false,
            ..Default::default()
        };
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "parent".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: crate::orchestration::InheritedPermissions::new(
                crate::orchestration::permission_sync::PermissionMode::Deny,
            ),
            inherited_skills: vec![],
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            spawn_tool_call_id: None,
            execution_metadata: None,
            delegation_chain: Vec::new(),
        };

        let result = spawner.spawn(input, &context).await.unwrap();
        assert!(matches!(result, SpawnAgentOutput::Completed { .. }));

        let mode = executor.take_captured().expect("executor captured config");
        assert_eq!(
            mode,
            crate::orchestration::permission_sync::PermissionMode::Deny
        );
    }

    #[tokio::test]
    async fn spawn_without_executor_fails_without_reserving_state() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let input = SpawnAgentInput {
            description: "Test agent".to_string(),
            prompt: "Do a test".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: true,
            fanout_group_id: Some("review-1".to_string()),
            fanout_target_count: Some(2),
            fanout_slot_index: Some(0),
            ..Default::default()
        };

        let result = spawner.spawn(input, &make_bg_context()).await;
        assert!(
            matches!(result, Err(SpawnError::ExecutorUnavailable)),
            "missing executor must fail explicitly, got {result:?}"
        );
        assert!(
            spawner.active_agents.read().await.is_empty(),
            "missing executor must not leave fake active agents"
        );
        assert!(
            spawner.completion_notifiers.read().await.is_empty(),
            "missing executor must not leave completion notifiers"
        );
        assert_eq!(
            spawner.background_task_count(),
            0,
            "missing executor must not enqueue a background task"
        );
        assert!(
            spawner.list_fanout_groups().await.is_empty(),
            "missing executor must not leave fanout rows for a child that never ran"
        );
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
            inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
            inherited_skills: vec![],
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            spawn_tool_call_id: None,
            execution_metadata: None,
            delegation_chain: Vec::new(),
        };

        let result = spawner.spawn(input, &context).await;
        assert!(matches!(result, Err(SpawnError::UnknownAgentType(_))));
    }

    #[test]
    fn agent_status_to_progress_event_emits_interrupted_terminal_status() {
        let event = agent_status_to_progress_event(
            &AgentStatus::Interrupted {
                partial_result: "partial".to_string(),
                finish_reason: "budget_exhausted".to_string(),
            },
            &SpawnedAgentMetrics {
                tool_calls: 2,
                prompt_tokens: 10,
                completion_tokens: 5,
                ..Default::default()
            },
            SystemTime::now(),
        )
        .expect("interrupted status should emit progress");

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
        let factory = BlockingExecutorFactory::new();
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(factory.clone() as Arc<dyn SpawnAgentExecutor>);
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "parent".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
            inherited_skills: vec![],
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            spawn_tool_call_id: None,
            execution_metadata: None,
            delegation_chain: Vec::new(),
        };

        let input = SpawnAgentInput {
            description: "Agent 1".to_string(),
            prompt: "Test".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: true,
            ..Default::default()
        };
        let _ = spawner.spawn(input, &context).await.unwrap();

        let agents = spawner.list_agents("parent-123").await;
        assert_eq!(agents.len(), 1);

        factory.unblock();
        spawner
            .shutdown_and_wait(std::time::Duration::from_secs(2))
            .await;
    }

    #[tokio::test]
    async fn test_context_cache_shared_across_spawns() {
        use astra_turn_core::orchestration_context_cache::SharedContextCache;

        // Create a shared context cache
        let cache = Arc::new(SharedContextCache::default());

        // Create spawner with custom cache
        let spawner = DynamicAgentSpawner::with_context_cache(mock_router(), Arc::clone(&cache))
            .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>);

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
            inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
            inherited_skills: vec![],
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            spawn_tool_call_id: None,
            execution_metadata: None,
            delegation_chain: Vec::new(),
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
        let factory = BlockingExecutorFactory::new();
        let spawner = DynamicAgentSpawner::new(router.clone())
            .with_executor(factory.clone() as Arc<dyn SpawnAgentExecutor>);
        let mut parent_mailbox = router
            .register(AgentAddress::new("parent-123", "main"), None)
            .await
            .unwrap();
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "main".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            spawn_tool_call_id: None,
            execution_metadata: None,
            delegation_chain: Vec::new(),
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

        factory.unblock();
        spawner
            .shutdown_and_wait(std::time::Duration::from_secs(2))
            .await;
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

    struct CapturingPermissionExecutor {
        captured: std::sync::Mutex<Option<crate::orchestration::permission_sync::PermissionMode>>,
    }

    #[derive(Default)]
    struct SessionBindingExecutor {
        sessions: std::sync::Mutex<Vec<String>>,
    }

    struct ReorderedSessionBindingExecutor {
        sessions: std::sync::Mutex<Vec<String>>,
        first_callback_entered: std::sync::Barrier,
        release_first_callback: std::sync::Barrier,
        delay_first_callback: std::sync::atomic::AtomicBool,
    }

    impl ReorderedSessionBindingExecutor {
        fn new() -> Self {
            Self {
                sessions: std::sync::Mutex::new(Vec::new()),
                first_callback_entered: std::sync::Barrier::new(2),
                release_first_callback: std::sync::Barrier::new(2),
                delay_first_callback: std::sync::atomic::AtomicBool::new(true),
            }
        }
    }

    #[async_trait]
    impl SpawnAgentExecutor for SessionBindingExecutor {
        fn bind_parent_session(&self, session_id: &str) {
            self.sessions.lock().unwrap().push(session_id.to_string());
        }

        async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            Err("session binding test executor does not execute runs".to_string())
        }
    }

    #[async_trait]
    impl SpawnAgentExecutor for ReorderedSessionBindingExecutor {
        fn bind_parent_session(&self, session_id: &str) {
            if session_id == "session-old"
                && self
                    .delay_first_callback
                    .swap(false, std::sync::atomic::Ordering::AcqRel)
            {
                self.first_callback_entered.wait();
                self.release_first_callback.wait();
            }
            self.sessions.lock().unwrap().push(session_id.to_string());
        }

        async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            Err("session binding test executor does not execute runs".to_string())
        }
    }

    #[test]
    fn session_binding_is_idempotent_and_builder_order_independent() {
        let executor = Arc::new(SessionBindingExecutor::default());
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(executor.clone())
            .with_session("session-a".to_string());

        spawner.bind_session("session-a");
        spawner.bind_session("session-b");
        assert_eq!(
            *executor.sessions.lock().unwrap(),
            ["session-a", "session-b"],
            "same-session rebinding must not repeat executor side effects"
        );

        let late_executor = Arc::new(SessionBindingExecutor::default());
        let _spawner = DynamicAgentSpawner::new(mock_router())
            .with_session("session-before-executor".to_string())
            .with_executor(late_executor.clone());
        assert_eq!(
            *late_executor.sessions.lock().unwrap(),
            ["session-before-executor"],
            "installing an executor after the session must still bind it exactly once"
        );
    }

    #[test]
    fn concurrent_session_binding_reconciles_executor_to_the_latest_identity() {
        let executor = Arc::new(ReorderedSessionBindingExecutor::new());
        let spawner =
            Arc::new(DynamicAgentSpawner::new(mock_router()).with_executor(executor.clone()));
        let first_spawner = Arc::clone(&spawner);
        let first = std::thread::spawn(move || first_spawner.bind_session("session-old"));

        executor.first_callback_entered.wait();
        spawner.bind_session("session-new");
        executor.release_first_callback.wait();
        first.join().unwrap();

        assert_eq!(spawner.current_session_id().as_deref(), Some("session-new"));
        assert_eq!(
            executor.sessions.lock().unwrap().last().map(String::as_str),
            Some("session-new"),
            "a delayed stale callback must reconcile back to the latest session"
        );
    }

    impl CapturingDepthExecutor {
        fn new() -> Self {
            Self {
                captured_depth: std::sync::Mutex::new(None),
            }
        }
    }

    impl CapturingPermissionExecutor {
        fn new() -> Self {
            Self {
                captured: std::sync::Mutex::new(None),
            }
        }

        fn take_captured(&self) -> Option<crate::orchestration::permission_sync::PermissionMode> {
            self.captured.lock().unwrap().take()
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
                cancelled_by_user: None,
                output: Some("ok".into()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
                turns_completed: 0,
                permission_summary: None,
                permission_requests: 0,
                permission_requests_approved: 0,
                tools_blocked: 0,
            })
        }
    }

    #[async_trait]
    impl SpawnAgentExecutor for CapturingPermissionExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            let mode = config.permission_context.read().await.mode();
            *self.captured.lock().unwrap() = Some(mode);
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "completed".into(),
                finish_reason: "normal".into(),
                cancelled_by_user: None,
                output: Some("ok".into()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
                turns_completed: 0,
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
                cancelled_by_user: None,
                output: Some("ok".into()),
                error: None,
                prompt_tokens: 1,
                completion_tokens: 1,
                tool_calls: 0,
                turns_completed: 3,
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
                cancelled_by_user: None,
                output: self.output.map(str::to_string),
                error: self.error.map(str::to_string),
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
                turns_completed: 0,
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
                cancelled_by_user: None,
                output: Some("ok".into()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
                turns_completed: 0,
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
            cancelled_by_user: None,
            output: Some("partial".into()),
            error: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
            turns_completed: 0,
            permission_summary: None,
            permission_requests: 0,
            permission_requests_approved: 0,
            tools_blocked: 0,
        };
        assert!(matches!(
            spawn_run_result_to_agent_status(&interrupted),
            AgentStatus::Interrupted {
                ref partial_result,
                ref finish_reason,
            } if partial_result == "partial" && finish_reason == "budget_exhausted"
        ));
        assert!(matches!(
            spawn_run_result_to_sync_output("a1".into(), "run-a1".into(), interrupted.clone(), 12),
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
            spawn_run_result_to_sync_output("a1".into(), "run-a1".into(), unknown, 12),
            SpawnAgentOutput::Failed { .. }
        ));

        let waiting = SpawnRunResult {
            status: SPAWN_STATUS_WAITING.into(),
            finish_reason: "waiting".into(),
            output: Some("executor_offline".into()),
            ..interrupted
        };
        assert!(matches!(
            spawn_run_result_to_agent_status(&waiting),
            AgentStatus::Waiting { ref reason } if reason == "executor_offline"
        ));
        assert!(matches!(
            agent_status_to_progress_event(
                &spawn_run_result_to_agent_status(&waiting),
                &SpawnedAgentMetrics::default(),
                SystemTime::now(),
            ),
            Some(ProgressEventType::Waiting { reason }) if reason == "executor_offline"
        ));
    }

    #[test]
    fn dropped_terminal_output_is_internal_failure_not_user_cancel() {
        let output = dropped_agent_terminal_output("agent-42", "run-42", 500);
        assert!(
            matches!(
                output,
                SpawnAgentOutput::Failed {
                    agent_id: _,
                    ref run_id,
                    ref error,
                    ref finish_reason,
                    duration_ms: 500
                } if finish_reason == "executor_dropped"
                    && run_id == "run-42"
                    && error.contains("agent-42")
                    && error.contains("no completion payload")
            ),
            "dropped terminal sender must be diagnosed as internal executor loss, got {output:?}"
        );
    }

    #[test]
    fn fanout_slot_status_counts_only_budget_interruptions_as_parent_budget_cancel() {
        let budget_interrupted = AgentStatus::Interrupted {
            partial_result: "partial review".to_string(),
            finish_reason: "budget_exhausted".to_string(),
        };
        let projection = project_agent_status_to_fanout_slot(&budget_interrupted);
        assert_eq!(
            projection.status,
            AgentFanoutSlotStatus::CancelledByParentBudget
        );
        assert_eq!(
            projection.terminal_reason.as_deref(),
            Some("budget_exhausted")
        );

        let empty_completion = AgentStatus::Interrupted {
            partial_result: String::new(),
            finish_reason: "empty_completion".to_string(),
        };
        let projection = project_agent_status_to_fanout_slot(&empty_completion);
        assert_eq!(
            projection.status,
            AgentFanoutSlotStatus::Interrupted,
            "non-budget interrupted child runs are first-class interrupted slots, not completed or failed"
        );
        assert_eq!(
            projection.terminal_reason.as_deref(),
            Some("empty_completion")
        );
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
        assert_eq!(
            spawn_completion_status_from_finish_reason(Some(
                astra_turn_core::response_guard::RESPONSE_GUARD_REDACTED_FINISH_REASON
            )),
            "completed"
        );
    }

    #[test]
    fn spawn_run_result_keeps_safety_redacted_fallback_completed() {
        let status = spawn_run_result_to_agent_status(&SpawnRunResult {
            agent_id: "agent-1".into(),
            run_id: "run-1".into(),
            status: SPAWN_STATUS_COMPLETED.into(),
            finish_reason: astra_turn_core::response_guard::RESPONSE_GUARD_REDACTED_FINISH_REASON
                .into(),
            cancelled_by_user: None,
            output: Some(astra_turn_core::response_guard::INTERNAL_PROTOCOL_FALLBACK.into()),
            error: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
            turns_completed: 0,
            permission_summary: None,
            permission_requests: 0,
            permission_requests_approved: 0,
            tools_blocked: 0,
        });

        match status {
            AgentStatus::Completed {
                result,
                finish_reason,
            } => {
                assert_eq!(
                    result,
                    astra_turn_core::response_guard::INTERNAL_PROTOCOL_FALLBACK
                );
                assert_eq!(
                    finish_reason.as_deref(),
                    Some(astra_turn_core::response_guard::RESPONSE_GUARD_REDACTED_FINISH_REASON)
                );
            }
            other => panic!("expected completed status, got {other:?}"),
        }
    }

    #[test]
    fn subrun_status_projection_maps_interruption_cancel_and_unknown_via_spawn_owner() {
        let paused = project_subrun_status_to_spawn(astra_core::STATUS_PAUSED, None);
        assert_eq!(paused.status, SPAWN_STATUS_INTERRUPTED);
        assert_eq!(paused.finish_reason, "paused");
        assert!(paused.error.is_none());

        let partial = project_subrun_status_to_spawn(
            astra_services::coordination::AGENT_RESULT_STATUS_PARTIAL,
            Some("budget_exhausted: adaptive hard turn limit reached".to_string()),
        );
        assert_eq!(partial.status, SPAWN_STATUS_INTERRUPTED);
        assert_eq!(
            partial.finish_reason,
            "budget_exhausted: adaptive hard turn limit reached"
        );
        assert!(
            partial.error.is_none(),
            "an interruption reason is evidence, not a spawn failure"
        );

        let waiting = project_subrun_status_to_spawn(astra_core::STATUS_WAITING, None);
        assert_eq!(waiting.status, SPAWN_STATUS_WAITING);
        assert_eq!(waiting.finish_reason, SPAWN_STATUS_WAITING);

        let cancelled = project_subrun_status_to_spawn(astra_core::STATUS_CANCELLED, None);
        assert_eq!(cancelled.status, SPAWN_STATUS_CANCELLED);
        assert_eq!(cancelled.finish_reason, SPAWN_STATUS_CANCELLED);

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
            inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            spawn_tool_call_id: None,
            execution_metadata: None,
            delegation_chain: Vec::new(),
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
        let agent_id = match spawner.spawn(input, &context).await.unwrap() {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched, got {other:?}"),
        };

        // Mailbox should be unregistered after completion.
        for _ in 0..20 {
            if router
                .list_registered_agents(&context.parent_run_id)
                .await
                .is_ok_and(|agents| agents.is_empty())
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        assert!(
            router
                .list_registered_agents(&context.parent_run_id)
                .await
                .is_ok_and(|agents| agents.is_empty()),
            "background completion should unregister mailbox"
        );
        let completed = spawner
            .get_agent_state_any(&agent_id)
            .await
            .expect("completed child should remain inspectable");
        assert_eq!(
            completed.metrics.turns_completed, 3,
            "executor-reported loop rounds must survive archival and journaling"
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
            inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            spawn_tool_call_id: None,
            execution_metadata: None,
            delegation_chain: Vec::new(),
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
            inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            spawn_tool_call_id: None,
            execution_metadata: None,
            delegation_chain: Vec::new(),
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
            recursion_depth:
                astra_turn_core::agentic_recursion_guard::ABSOLUTE_MAX_AGENT_RECURSION_DEPTH,
            parent_is_fork_child: false,
            inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            spawn_tool_call_id: None,
            execution_metadata: None,
            delegation_chain: Vec::new(),
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
            inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            spawn_tool_call_id: None,
            execution_metadata: None,
            delegation_chain: Vec::new(),
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

        let mut context = make_bg_context();
        context.execution_metadata = Some(serde_json::json!({
            "workspace": {
                "kind": "edge_workspace",
                "cwd": "/Users/xupeng/github/astra"
            },
            "executor": {
                "kind": "edge_agent",
                "executor_id": "edge-macbook-1",
                "transport": "edge_ws"
            },
            "transport": "edge_ws"
        }));

        let launched = spawner.spawn(make_bg_input(), &context).await.unwrap();
        let agent_id = match launched {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched output, got {other:?}"),
        };

        let status = spawner
            .wait_for_agent(&agent_id, Duration::from_secs(1))
            .await
            .expect("background agent should complete");
        assert!(matches!(
            status,
            AgentStatus::Interrupted {
                partial_result,
                finish_reason
            } if partial_result == "partial" && finish_reason == "budget_exhausted"
        ));

        let journal_path = astra_services::session_journal::journal_file_path("sess-123");
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
        assert!(journal.contains("\"kind\":\"edge_workspace\""), "{journal}");
        assert!(
            journal.contains("\"executor_id\":\"edge-macbook-1\""),
            "{journal}"
        );
        assert!(journal.contains("\"transport\":\"edge_ws\""), "{journal}");
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
            inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            spawn_tool_call_id: None,
            execution_metadata: None,
            delegation_chain: Vec::new(),
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
    async fn sync_spawn_waiting_emits_waiting_progress_and_archives_status() {
        let spawner = DynamicAgentSpawner::new(mock_router()).with_executor(Arc::new(
            ImmediateStatusExecutor {
                status: SPAWN_STATUS_WAITING,
                finish_reason: "waiting",
                output: Some("executor_offline"),
                error: None,
            },
        ));
        let mut progress = spawner.subscribe_progress();
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "main".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp"),
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            spawn_tool_call_id: None,
            execution_metadata: None,
            delegation_chain: Vec::new(),
        };
        let input = SpawnAgentInput {
            description: "Waiting status".to_string(),
            prompt: "Should wait".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: false,
            ..Default::default()
        };

        let result = spawner.spawn(input, &context).await.unwrap();
        assert!(matches!(
            result,
            SpawnAgentOutput::Waiting { ref reason, .. } if reason == "executor_offline"
        ));
        let completed = spawner.completed_agents.read().await;
        assert!(matches!(
            completed.front().map(|state| &state.status),
            Some(AgentStatus::Waiting { reason }) if reason == "executor_offline"
        ));
        drop(completed);

        let mut saw_waiting = false;
        while let Ok(event) = progress.try_recv() {
            if matches!(
                event.event_type,
                ProgressEventType::Waiting { ref reason } if reason == "executor_offline"
            ) {
                saw_waiting = true;
                break;
            }
        }
        assert!(
            saw_waiting,
            "sync waiting spawn must emit agent_waiting progress"
        );
    }

    #[tokio::test]
    async fn test_inherited_skills_passed_to_run_config() {
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>);
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "parent".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
            inherited_skills: vec!["review-changes".to_string(), "analyze-session".to_string()],
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            spawn_tool_call_id: None,
            execution_metadata: None,
            delegation_chain: Vec::new(),
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
            inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
            inherited_skills: Vec::new(),
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            spawn_tool_call_id: None,
            execution_metadata: None,
            delegation_chain: Vec::new(),
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
                cancelled_by_user: None,
                output: Some("done".into()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
                turns_completed: 0,
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
            inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
            inherited_skills: vec![],
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            spawn_tool_call_id: None,
            execution_metadata: None,
            delegation_chain: Vec::new(),
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

    fn completed_test_state(index: usize) -> SpawnedAgentState {
        SpawnedAgentState {
            agent_id: format!("agent-{index}"),
            run_id: format!("run-{index}"),
            parent_run_id: "root".to_string(),
            agent_type: "explore".to_string(),
            description: format!("archived {index}"),
            status: AgentStatus::Completed {
                result: "ok".to_string(),
                finish_reason: Some("normal".to_string()),
            },
            work_revision: 1,
            messaging_address: None,
            worktree_path: None,
            started_at: SystemTime::now(),
            ended_at: Some(SystemTime::now()),
            metrics: Default::default(),
            permission_summary: PermissionSummary::default(),
            parent_agent_id: "parent".to_string(),
            trace_context: None,
            spawn_tool_call_id: None,
            run_in_background: true,
            fanout_slot: None,
            execution_metadata: None,
        }
    }

    #[test]
    fn background_agent_publishes_one_monotonic_session_work_unit() {
        use astra_core::work_unit::{ActiveWorkRegistry, WorkUnitStatus, WorkUnitWakePolicy};

        let registry = Arc::new(ActiveWorkRegistry::default());
        let spawner =
            DynamicAgentSpawner::new(mock_router()).with_active_work_registry(registry.clone());
        let mut state = completed_test_state(7);
        state.status = AgentStatus::Running {
            activity: "reviewing".into(),
        };
        state.ended_at = None;
        state.work_revision = 1;
        spawner.publish_background_agent(&state);
        let running = registry.active_work_observations();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].kind, "agent");
        assert_eq!(running[0].status, WorkUnitStatus::Running);
        assert_eq!(
            running[0].wake_policy,
            WorkUnitWakePolicy::OnAttentionOrTerminal
        );

        state.status = AgentStatus::Waiting {
            reason: "need a contract decision".into(),
        };
        state.work_revision = 2;
        spawner.publish_background_agent(&state);
        assert_eq!(
            registry.active_work_observations()[0].status,
            WorkUnitStatus::WaitingForInput
        );

        state.status = AgentStatus::Completed {
            result: "done".into(),
            finish_reason: Some("normal".into()),
        };
        state.work_revision = 3;
        spawner.publish_background_agent(&state);
        assert!(registry.active_work_observations().is_empty());
        assert_eq!(
            registry
                .terminal_observation(&state.agent_id, "agent")
                .unwrap()
                .revision,
            3
        );

        state.fanout_slot = AgentFanoutSlotIdentity::new("group", 1, 0, None).ok();
        state.agent_id = "fanout-child".into();
        spawner.publish_background_agent(&state);
        assert!(
            registry
                .canonical_observation("fanout-child", "agent")
                .is_none()
        );
    }

    #[tokio::test]
    async fn fanout_group_limit_rejected_when_all_live() {
        // When the fanout-groups map is at MAX_FANOUT_GROUPS and every group
        // is still live (Running), eviction has no terminal candidate and
        // must surface FanoutGroupLimitExceeded instead of silently growing
        // the map or evicting a live group.
        let spawner = DynamicAgentSpawner::new(mock_router());
        {
            let mut groups = spawner.fanout_groups.write().await;
            for i in 0..MAX_FANOUT_GROUPS {
                let mut g = AgentFanoutGroupProjection::new(
                    format!("live-group-{i}"),
                    format!("live {i}"),
                    1,
                );
                g.status = AgentFanoutStatus::Running;
                groups.insert(format!("live-group-{i}"), g);
            }
        }
        let mut groups = spawner.fanout_groups.write().await;
        let result = spawner.evict_terminal_fanout_group_if_full(&mut groups);
        match result {
            Err(SpawnError::FanoutGroupLimitExceeded { active, limit }) => {
                assert_eq!(active, MAX_FANOUT_GROUPS);
                assert_eq!(limit, MAX_FANOUT_GROUPS);
            }
            other => panic!("expected FanoutGroupLimitExceeded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fanout_group_evicts_oldest_terminal_when_full() {
        // At capacity with a mix of live and terminal groups, eviction must
        // pick the terminal group with the oldest last_touched and leave all
        // live groups untouched.
        let spawner = DynamicAgentSpawner::new(mock_router());
        let old = SystemTime::now() - Duration::from_secs(120);
        {
            let mut groups = spawner.fanout_groups.write().await;
            for i in 0..(MAX_FANOUT_GROUPS - 1) {
                let mut g =
                    AgentFanoutGroupProjection::new(format!("live-{i}"), format!("live {i}"), 1);
                g.status = AgentFanoutStatus::Running;
                groups.insert(format!("live-{i}"), g);
            }
            // One terminal group, touched long ago — the eviction target.
            let mut terminal = AgentFanoutGroupProjection::new("terminal-old", "terminal old", 1);
            terminal.status = AgentFanoutStatus::Finished;
            terminal.last_touched = old;
            groups.insert("terminal-old".to_string(), terminal);
        }
        let mut groups = spawner.fanout_groups.write().await;
        let evicted = spawner
            .evict_terminal_fanout_group_if_full(&mut groups)
            .expect("terminal candidate exists, eviction should succeed");
        assert_eq!(evicted.len(), 0, "terminal group had no settled agents");
        assert!(!groups.contains_key("terminal-old"));
        assert_eq!(groups.len(), MAX_FANOUT_GROUPS - 1);
    }

    #[tokio::test]
    async fn completed_agent_archive_evicts_oldest_at_capacity() {
        let spawner = DynamicAgentSpawner::new(mock_router());

        for index in 0..260 {
            spawner.archive_state(completed_test_state(index)).await;
        }

        let completed = spawner.completed_agents.read().await;
        assert_eq!(completed.len(), 256);
        assert_eq!(completed.front().unwrap().agent_id, "agent-4");
        assert_eq!(completed.back().unwrap().agent_id, "agent-259");
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

        // Once the first two finish naturally, a fresh spawn is again
        // accepted — the cap is a live measurement, not a one-way counter.
        // Do not use `shutdown_and_wait` as a generic drain primitive: by
        // contract shutdown permanently closes this runtime's admission.
        factory2.unblock();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !spawner.list_all_agents().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("initial agents should finish after their executor is released");
        let after_drain = spawner.spawn(make_bg_input(), &make_bg_context()).await;
        assert!(
            after_drain.is_ok(),
            "after drain the cap must accept new spawns again, got {after_drain:?}"
        );
        spawner
            .shutdown_and_wait(std::time::Duration::from_secs(2))
            .await;
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
    async fn direct_spawn_is_blocked_only_for_parent_run_that_declared_fanout() {
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>);

        spawner
            .declare_fanout_group("fanout-a", "fanout A", 2, Some("call-a"), "parent-a")
            .await
            .expect("declaring a fanout group should succeed");

        let mut same_parent = make_bg_context();
        same_parent.parent_run_id = "parent-a".to_string();
        let blocked = spawner.spawn(make_bg_input(), &same_parent).await;
        let err = blocked.expect_err("same parent run must not bypass fanout with direct spawn");
        let message = err.to_string();
        assert!(message.contains("already used agent_fanout"), "{message}");
        assert!(message.contains("fanout-a"), "{message}");

        let cross_parent_reuse = spawner
            .declare_fanout_group("fanout-a", "fanout A", 2, Some("call-b"), "parent-b")
            .await;
        let err = cross_parent_reuse.expect_err("group ids must not be reused across parent runs");
        let message = err.to_string();
        assert!(
            message.contains("belongs to parent_run_id 'parent-a'"),
            "{message}"
        );

        let mut other_parent = make_bg_context();
        other_parent.parent_run_id = "parent-b".to_string();
        let allowed = spawner.spawn(make_bg_input(), &other_parent).await;
        assert!(
            matches!(allowed, Ok(SpawnAgentOutput::Launched { .. })),
            "a different parent run must not inherit another run's fanout gate: {allowed:?}"
        );
        spawner
            .shutdown_and_wait(std::time::Duration::from_secs(2))
            .await;
    }

    #[tokio::test]
    async fn fanout_active_slot_cache_follows_state_transition_delta_once() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let identity =
            AgentFanoutSlotIdentity::new("review-cache", 1, 0, Some("storage".to_string()))
                .unwrap();

        spawner
            .record_fanout_spawn_accepted(
                &identity,
                Some("review cache"),
                "storage@run-1",
                "run-1",
                "explore",
                "review storage",
                Some("call-1"),
                "parent-123",
            )
            .await
            .unwrap();
        assert_eq!(
            spawner
                .cached_active_fanout_slots
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "accepted running slot increments active cache"
        );

        let mut state = completed_test_state(1);
        state.agent_id = "storage@run-1".to_string();
        state.fanout_slot = Some(identity);
        spawner.record_fanout_terminal_state(&state).await;
        assert_eq!(
            spawner
                .cached_active_fanout_slots
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "running -> terminal decrements active cache"
        );

        spawner.record_fanout_terminal_state(&state).await;
        assert_eq!(
            spawner
                .cached_active_fanout_slots
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "duplicate terminal recording must not decrement the cache again"
        );
    }

    #[tokio::test]
    async fn spawned_agent_state_preserves_fanout_slot_identity() {
        let factory = BlockingExecutorFactory::new();
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(factory.clone() as Arc<dyn SpawnAgentExecutor>);
        let mut input = make_bg_input();
        input.fanout_group_id = Some("review-1".to_string());
        input.fanout_group_title = Some("review fanout".to_string());
        input.fanout_target_count = Some(3);
        input.fanout_slot_index = Some(1);
        input.fanout_slot_id = Some("storage".to_string());

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
            .expect("spawned agent should remain active while executor is running");
        let slot = state
            .fanout_slot
            .as_ref()
            .expect("state should keep fanout slot identity");
        assert_eq!(slot.group_id, "review-1");
        assert_eq!(slot.target_count, 3);
        assert_eq!(slot.slot_index, 1);
        assert_eq!(slot.slot_id.as_deref(), Some("storage"));

        let listed = spawner.list_agents("root").await;
        let projected = listed
            .iter()
            .find(|info| info.agent_id == agent_id)
            .expect("list projection should include spawned agent");
        assert_eq!(projected.fanout_slot, state.fanout_slot);

        factory.unblock();
        spawner
            .shutdown_and_wait(std::time::Duration::from_secs(2))
            .await;
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
        let factory = BlockingExecutorFactory::new();
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(factory.clone() as Arc<dyn SpawnAgentExecutor>);
        let mut input = make_bg_input();
        input.fanout_group_id = Some("review-1".to_string());
        input.fanout_group_title = Some("review fanout".to_string());
        input.fanout_target_count = Some(3);
        input.fanout_slot_index = Some(1);
        input.fanout_slot_id = Some("storage".to_string());

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
        assert_eq!(groups[0].slots[1].slot_id.as_deref(), Some("storage"));
        assert_eq!(groups[0].slots[1].requested_description, "bg test");

        factory.unblock();
        spawner
            .shutdown_and_wait(std::time::Duration::from_secs(2))
            .await;
    }

    #[tokio::test]
    async fn rejected_fanout_spawn_cleans_created_worktree() {
        let temp = tempfile::TempDir::new().unwrap();
        let factory = BlockingExecutorFactory::new();
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(factory.clone() as Arc<dyn SpawnAgentExecutor>);
        let mut context = make_bg_context();
        context.working_dir = temp.path().to_path_buf();
        let mut input = make_bg_input();
        input.isolated = true;
        input.fanout_group_id = Some("review-cleanup".to_string());
        input.fanout_group_title = Some("review cleanup".to_string());
        input.fanout_target_count = Some(2);
        input.fanout_slot_index = Some(0);

        let first = spawner
            .spawn(input.clone(), &context)
            .await
            .expect("first isolated fanout spawn should be accepted");
        assert!(matches!(first, SpawnAgentOutput::Launched { .. }));
        let worktree_base = temp.path().join(".agent-worktrees");
        let count_worktrees = || -> usize {
            std::fs::read_dir(&worktree_base)
                .unwrap()
                .filter_map(Result::ok)
                .count()
        };
        assert_eq!(count_worktrees(), 1);

        let duplicate = spawner.spawn(input, &context).await;
        assert!(
            matches!(duplicate, Err(SpawnError::InvalidInput(ref message)) if message.contains("already accepted")),
            "duplicate slot must reject after worktree creation: {duplicate:?}"
        );
        assert_eq!(
            count_worktrees(),
            1,
            "duplicate fanout rejection must remove the worktree it created"
        );
        assert_eq!(spawner.active_agents.read().await.len(), 1);

        factory.unblock();
        spawner
            .shutdown_and_wait(std::time::Duration::from_secs(2))
            .await;
    }

    #[tokio::test]
    async fn fanout_group_lookup_by_agent_uses_spawn_index() {
        let factory = BlockingExecutorFactory::new();
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(factory.clone() as Arc<dyn SpawnAgentExecutor>);
        let mut input = make_bg_input();
        input.fanout_group_id = Some("review-lookup".to_string());
        input.fanout_group_title = Some("review lookup".to_string());
        input.fanout_target_count = Some(2);
        input.fanout_slot_index = Some(0);

        let agent_id = match spawner.spawn(input, &make_bg_context()).await.unwrap() {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched output, got {other:?}"),
        };
        let group = spawner
            .fanout_group_for_agent(&agent_id)
            .await
            .expect("spawned fanout child should have indexed group");
        assert_eq!(group.group_id, "review-lookup");
        assert_eq!(group.slots[0].agent_id.as_deref(), Some(agent_id.as_str()));
        assert!(
            spawner
                .fanout_group_for_agent("missing-agent")
                .await
                .is_none()
        );

        factory.unblock();
        spawner
            .shutdown_and_wait(std::time::Duration::from_secs(2))
            .await;
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
        assert!(
            completed[0].ended_at.is_some(),
            "terminal archive must carry the runtime-owned end time"
        );
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
                .promote_foreground_work_to_background(Some("not-this-parent"))
                .await
                .is_empty(),
            "promotion must respect the parent run filter"
        );

        let promoted = spawner
            .promote_foreground_work_to_background(Some("root"))
            .await;
        assert_eq!(promoted.len(), 1, "foreground sync agent is one work item");
        let promoted = &promoted[0];
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
                .promote_foreground_work_to_background(Some("root"))
                .await
                .is_empty(),
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
    async fn local_user_guidance_uses_the_owned_agent_mailbox_identity() {
        struct CaptureMailbox {
            sender: std::sync::Mutex<
                Option<tokio::sync::oneshot::Sender<astra_messaging::AgentMailbox>>,
            >,
        }

        #[async_trait]
        impl SpawnAgentExecutor for CaptureMailbox {
            async fn execute(&self, mut config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
                let mailbox = config.mailbox.take().expect("spawned agent mailbox");
                if let Some(sender) = self.sender.lock().unwrap().take() {
                    let _ = sender.send(mailbox);
                }
                std::future::pending::<Result<SpawnRunResult, String>>().await
            }
        }

        let (mailbox_tx, mailbox_rx) = tokio::sync::oneshot::channel();
        let executor = Arc::new(CaptureMailbox {
            sender: std::sync::Mutex::new(Some(mailbox_tx)),
        });
        let spawner = Arc::new(
            DynamicAgentSpawner::new(mock_router())
                .with_executor(executor as Arc<dyn SpawnAgentExecutor>),
        );
        let spawn_task = {
            let spawner = Arc::clone(&spawner);
            tokio::spawn(async move { spawner.spawn(make_sync_input(), &make_bg_context()).await })
        };
        let mailbox = tokio::time::timeout(Duration::from_secs(1), mailbox_rx)
            .await
            .expect("executor should receive mailbox")
            .expect("mailbox sender should stay alive");
        let agent_id = mailbox.address.agent_id.clone();

        spawner
            .guide_agent(&agent_id, "guide-1", "inspect the storage race")
            .await
            .expect("active local agent should accept mailbox guidance");
        let message = tokio::time::timeout(Duration::from_secs(1), mailbox.recv())
            .await
            .expect("guidance should reach the child mailbox")
            .expect("child mailbox should remain open");
        assert_eq!(message.id, "guide-1");
        assert!(message.requires_ack);
        assert!(matches!(
            &message.to,
            astra_messaging::MessageTarget::Direct { address }
                if address == &mailbox.address
        ));
        assert!(matches!(
            &message.payload,
            astra_messaging::MessagePayload::Text { content, summary }
                if content == "inspect the storage race" && summary.as_deref() == Some("User guidance")
        ));

        assert!(spawner.cancel_agent(&agent_id, "test cleanup").await);
        let _ = tokio::time::timeout(Duration::from_secs(1), spawn_task).await;
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
    /// reference-agent parity: background means "launch now, report later", so
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
        assert_eq!(
            bg_results,
            vec![(
                agent_id,
                "Agent cancelled: turn budget exhausted".to_string()
            )],
            "shutdown aggregation must surface cancellation instead of silently dropping it"
        );
    }

    #[tokio::test]
    async fn cancelling_parent_run_converges_the_entire_dynamic_agent_tree() {
        struct NeverCompletes;

        #[async_trait]
        impl SpawnAgentExecutor for NeverCompletes {
            async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
                std::future::pending::<Result<SpawnRunResult, String>>().await
            }
        }

        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(NeverCompletes) as Arc<dyn SpawnAgentExecutor>);

        let first = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .expect("first child should launch");
        let (first_agent_id, first_run_id) = match first {
            SpawnAgentOutput::Launched {
                agent_id, run_id, ..
            } => (agent_id, run_id),
            other => panic!("expected launched first child, got {other:?}"),
        };

        let mut nested_context = make_bg_context();
        nested_context.parent_run_id = first_run_id.clone();
        nested_context.parent_agent_id = first_agent_id.clone();
        nested_context.recursion_depth = 1;
        let nested = spawner
            .spawn(make_bg_input(), &nested_context)
            .await
            .expect("nested child should launch");
        let nested_agent_id = match nested {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched nested child, got {other:?}"),
        };

        assert_eq!(
            spawner
                .cancel_descendants_of_parent_run(
                    "root",
                    DescendantCancellationReason::AncestorCancelled,
                )
                .await,
            2
        );
        assert!(spawner.list_all_agents().await.is_empty());
        for agent_id in [&first_agent_id, &nested_agent_id] {
            let archived = spawner
                .get_agent_state_any(agent_id)
                .await
                .expect("cancelled descendant should remain queryable");
            assert!(matches!(
                archived.status,
                AgentStatus::Cancelled {
                    by_user: false,
                    ref reason,
                } if reason == DescendantCancellationReason::AncestorCancelled.as_str()
            ));
        }

        let rejected_root_child = spawner.spawn(make_bg_input(), &make_bg_context()).await;
        assert!(
            matches!(rejected_root_child, Err(SpawnError::Race(ref error)) if error.contains("parent run 'root' is cancelled")),
            "a cancelled parent must be fenced against late descendants: {rejected_root_child:?}"
        );

        let mut rejected_nested_context = make_bg_context();
        rejected_nested_context.parent_run_id = first_run_id;
        rejected_nested_context.parent_agent_id = first_agent_id.clone();
        rejected_nested_context.recursion_depth = 1;
        let rejected_nested = spawner
            .spawn(make_bg_input(), &rejected_nested_context)
            .await;
        assert!(
            matches!(rejected_nested, Err(SpawnError::Race(ref error)) if error.contains("descendant spawn rejected")),
            "every cancelled descendant run must also fence new children: {rejected_nested:?}"
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

    #[tokio::test]
    async fn background_progress_preserves_execution_metadata_for_get_result_path() {
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>);
        let mut progress = spawner.subscribe_progress();
        let mut context = make_bg_context();
        context.execution_metadata = Some(serde_json::json!({
            "workspace": {
                "kind": "edge_workspace",
                "cwd": "/Users/xupeng/github/astra"
            },
            "executor": {
                "kind": "edge_agent",
                "executor_id": "edge-macbook-1",
                "transport": "edge_ws"
            },
            "transport": "edge_ws"
        }));

        let result = spawner.spawn(make_bg_input(), &context).await.unwrap();
        let agent_id = match result {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched immediately, got {other:?}"),
        };

        let status = spawner
            .wait_for_agent_outcome(&agent_id, std::time::Duration::from_secs(5))
            .await;
        assert!(
            matches!(
                status,
                WaitForAgentOutcome::Status(AgentStatus::Completed { .. })
            ),
            "background get_result path must resolve completion; got {status:?}"
        );

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut saw_spawned = false;
        let mut saw_completed = false;
        while !(saw_spawned && saw_completed) {
            let event = tokio::time::timeout_at(deadline, progress.recv())
                .await
                .expect("timed out waiting for progress metadata")
                .expect("progress broadcaster closed");
            if event.agent_id != agent_id {
                continue;
            }
            let metadata = event.metadata.as_ref().expect("execution metadata");
            assert_eq!(metadata["workspace"]["kind"], "edge_workspace");
            assert_eq!(metadata["workspace"]["cwd"], "/Users/xupeng/github/astra");
            assert_eq!(metadata["executor"]["kind"], "edge_agent");
            assert_eq!(metadata["executor"]["executor_id"], "edge-macbook-1");
            assert_eq!(metadata["transport"], "edge_ws");
            match event.event_type {
                ProgressEventType::AgentSpawned { .. } => saw_spawned = true,
                ProgressEventType::Completed { .. } => saw_completed = true,
                _ => {}
            }
        }
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

    #[tokio::test]
    async fn dropping_root_spawner_aborts_the_task_tree_without_an_explicit_shutdown() {
        struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        struct DropAwarePendingExecutor {
            started: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
            dropped: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        }

        #[async_trait]
        impl SpawnAgentExecutor for DropAwarePendingExecutor {
            async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
                let guard = DropSignal(self.dropped.lock().unwrap().take());
                if let Some(started) = self.started.lock().unwrap().take() {
                    let _ = started.send(());
                }
                let result = std::future::pending::<Result<SpawnRunResult, String>>().await;
                drop(guard);
                result
            }
        }

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let spawner = DynamicAgentSpawner::new(mock_router()).with_executor(Arc::new(
            DropAwarePendingExecutor {
                started: std::sync::Mutex::new(Some(started_tx)),
                dropped: std::sync::Mutex::new(Some(dropped_tx)),
            },
        ));
        let launched = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .expect("pending child should be supervised");
        assert!(matches!(launched, SpawnAgentOutput::Launched { .. }));
        tokio::time::timeout(Duration::from_secs(1), started_rx)
            .await
            .expect("child executor should start")
            .expect("start signal should be delivered");

        // This is intentionally not `shutdown_and_wait`: the ownership
        // contract must remain safe on early returns, account replacement,
        // and panic unwinds that only drop the root runtime.
        drop(spawner);

        tokio::time::timeout(Duration::from_secs(1), dropped_rx)
            .await
            .expect("dropping the root must abort its pending task")
            .expect("pending executor future must be dropped");
    }

    #[tokio::test]
    async fn shutdown_closes_spawn_admission_permanently() {
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>);

        spawner.shutdown_and_wait(Duration::from_millis(10)).await;

        let result = spawner.spawn(make_bg_input(), &make_bg_context()).await;
        assert!(matches!(result, Err(SpawnError::LifecycleShuttingDown)));
        assert!(spawner.list_all_agents().await.is_empty());
        assert_eq!(spawner.background_task_count(), 0);
    }

    #[tokio::test]
    async fn shutdown_finalizes_a_foreground_child_instead_of_only_aborting_its_host() {
        struct NeverCompletes;

        #[async_trait]
        impl SpawnAgentExecutor for NeverCompletes {
            async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
                std::future::pending::<Result<SpawnRunResult, String>>().await
            }
        }

        let spawner = Arc::new(
            DynamicAgentSpawner::new(mock_router())
                .with_executor(Arc::new(NeverCompletes) as Arc<dyn SpawnAgentExecutor>),
        );
        let spawn_task = {
            let spawner = Arc::clone(&spawner);
            tokio::spawn(async move { spawner.spawn(make_sync_input(), &make_bg_context()).await })
        };

        let agent_id = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(agent) = spawner.list_all_agents().await.into_iter().next() {
                    break agent.agent_id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("foreground child should enter canonical active state");

        spawner
            .shutdown_and_wait_with_reason(Duration::from_millis(1), "test session shutdown")
            .await;

        assert!(spawner.list_all_agents().await.is_empty());
        let archived = spawner
            .get_agent_state_any(&agent_id)
            .await
            .expect("shutdown must archive the foreground child");
        assert!(matches!(
            archived.status,
            AgentStatus::Cancelled {
                by_user: false,
                ref reason,
            } if reason == "test session shutdown"
        ));
        let terminal = tokio::time::timeout(Duration::from_secs(1), spawn_task)
            .await
            .expect("foreground caller must be released")
            .expect("foreground spawn host must not panic")
            .expect("foreground spawn should return a terminal payload");
        assert!(matches!(terminal, SpawnAgentOutput::Failed { .. }));
    }

    #[tokio::test]
    async fn foreground_fanout_promotion_is_atomic_for_every_live_slot() {
        struct NeverCompletes;

        #[async_trait]
        impl SpawnAgentExecutor for NeverCompletes {
            async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
                std::future::pending::<Result<SpawnRunResult, String>>().await
            }
        }

        let spawner = Arc::new(
            DynamicAgentSpawner::new(mock_router())
                .with_executor(Arc::new(NeverCompletes) as Arc<dyn SpawnAgentExecutor>),
        );
        let mut spawn_tasks = Vec::new();
        for slot_index in 0..2 {
            let mut input = make_sync_input();
            input.description = format!("review slot {slot_index}");
            input.fanout_group_id = Some("review-group".into());
            input.fanout_group_title = Some("Review group".into());
            input.fanout_target_count = Some(2);
            input.fanout_slot_index = Some(slot_index);
            let spawner = Arc::clone(&spawner);
            spawn_tasks.push(tokio::spawn(async move {
                spawner.spawn(input, &make_bg_context()).await
            }));
        }

        for _ in 0..50 {
            if spawner.list_all_agents().await.len() == 2 {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(spawner.list_all_agents().await.len(), 2);

        let promoted = spawner
            .promote_foreground_work_to_background(Some("root"))
            .await;
        assert_eq!(promoted.len(), 2, "Ctrl+B must promote the whole fanout");
        assert!(promoted.iter().all(|agent| agent.run_in_background));
        assert_eq!(
            promoted
                .iter()
                .map(|agent| agent.fanout_slot.as_ref().unwrap().slot_index)
                .collect::<Vec<_>>(),
            vec![0, 1],
            "promotion result order follows durable fanout slot identity"
        );

        let promoted_ids = promoted
            .iter()
            .map(|agent| agent.agent_id.as_str())
            .collect::<HashSet<_>>();
        for task in spawn_tasks {
            let output = tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .expect("every fanout spawn wait must wake")
                .expect("spawn task must not panic")
                .expect("promoted spawn must succeed");
            assert!(
                matches!(output, SpawnAgentOutput::Launched { ref agent_id, .. } if promoted_ids.contains(agent_id.as_str())),
                "promoted fanout slot must return Launched: {output:?}"
            );
        }
        assert!(
            spawner
                .promote_foreground_work_to_background(Some("root"))
                .await
                .is_empty(),
            "an already-promoted group is idempotent"
        );

        for agent in promoted {
            assert!(spawner.cancel_agent(&agent.agent_id, "test cleanup").await);
        }
        spawner
            .shutdown_and_wait(std::time::Duration::from_secs(1))
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

    /// HIGH #5: background agent that panics must still finalize like a
    /// normal failed child, not just disappear from the JoinSet.
    #[tokio::test]
    async fn background_agent_panic_finalizes_failed_state() {
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(PanicExecutor) as Arc<dyn SpawnAgentExecutor>);

        let result = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();
        let agent_id = match result {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched, got {other:?}"),
        };

        let status = spawner
            .wait_for_agent(&agent_id, std::time::Duration::from_secs(2))
            .await;
        assert!(
            matches!(status, Some(AgentStatus::Failed { ref error, .. }) if error.contains("executor panicked")),
            "panic must surface as failed agent status, got {status:?}"
        );

        let archived = spawner
            .get_agent_state_any(&agent_id)
            .await
            .expect("panicked agent should remain queryable as archived failed state");
        assert!(
            matches!(archived.status, AgentStatus::Failed { .. }),
            "archived status must be failed: {:?}",
            archived.status
        );

        let _ = spawner
            .shutdown_and_wait(std::time::Duration::from_secs(2))
            .await;

        assert_eq!(
            spawner.background_task_count(),
            0,
            "panicked background task must not leave zombie in JoinSet"
        );
        assert!(
            spawner.active_agents.read().await.is_empty(),
            "panicked background task must not remain active"
        );
        assert!(
            spawner.completion_notifiers.read().await.is_empty(),
            "panicked background task must not leave completion notifiers"
        );
    }

    #[tokio::test]
    async fn foreground_agent_panic_returns_failed_and_archives() {
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(PanicExecutor) as Arc<dyn SpawnAgentExecutor>);
        let mut input = make_bg_input();
        input.run_in_background = false;

        let result = spawner.spawn(input, &make_bg_context()).await.unwrap();
        let agent_id = match result {
            SpawnAgentOutput::Failed { ref error, .. } if error.contains("executor panicked") => {
                spawner
                    .completed_agents
                    .read()
                    .await
                    .back()
                    .expect("foreground panic should archive failed state")
                    .agent_id
                    .clone()
            }
            other => panic!("expected foreground Failed output for panic, got {other:?}"),
        };

        let archived = spawner
            .get_agent_state_any(&agent_id)
            .await
            .expect("foreground panic should remain queryable");
        assert!(
            matches!(archived.status, AgentStatus::Failed { .. }),
            "archived status must be failed: {:?}",
            archived.status
        );
        assert!(
            spawner.active_agents.read().await.is_empty(),
            "foreground panic must not leave active agent state"
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
            inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
            inherited_skills: vec![],
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            spawn_tool_call_id: None,
            execution_metadata: None,
            delegation_chain: Vec::new(),
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
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>);
        let input = child_with_inherit(false);
        let ctx = parent_context("parent-unused");
        let result = spawner.spawn(input, &ctx).await;
        assert!(
            result.is_ok(),
            "spawn must succeed without store even when inherit_prefix is set, got {result:?}"
        );
    }

    #[tokio::test]
    async fn spawn_resolves_matching_captured_prefix() {
        let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
        let exec = Arc::new(CapturingPrefixExecutor::new());
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_prefix_store(store.clone())
            .with_executor(exec as Arc<dyn SpawnAgentExecutor>);
        capture_parent_for(&*store, "run-parent-A", TEST_CHILD_MODEL);

        let input = child_with_inherit(false);
        let ctx = parent_context("run-parent-A");
        let out = spawner.spawn(input, &ctx).await.unwrap();
        let agent_id = match out {
            SpawnAgentOutput::Completed { agent_id, .. } => agent_id,
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected successful spawn output, got {other:?}"),
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
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_prefix_store(store)
            .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>);
        let input = child_with_inherit(false); // not required
        let ctx = parent_context("run-no-capture");
        let out = spawner.spawn(input, &ctx).await.unwrap();
        let agent_id = match out {
            SpawnAgentOutput::Completed { agent_id, .. } => agent_id,
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected successful spawn output, got {other:?}"),
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
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_prefix_store(store)
            .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>);
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
            SpawnAgentOutput::Completed { agent_id, .. } => agent_id,
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected successful spawn output, got {other:?}"),
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
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>);
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
            result.is_ok(),
            "fork children must still be able to spawn ordinary non-inheriting children: {result:?}"
        );
    }

    #[tokio::test]
    async fn workspace_restore_turns_lost_live_fanout_into_collectable_partial_result() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let projection = astra_services::session_workspace::BackgroundLocalAgentTaskProjection {
            id: "reviewer@lost".into(),
            run_id: "run-reviewer-lost".into(),
            parent_run_id: "root".into(),
            status: "running".into(),
            title: "review storage".into(),
            started_at_ms: 42,
            ended_at_ms: None,
            output_tail: Some("partial finding".into()),
            terminal_reason: None,
            fanout: Some(
                astra_services::session_workspace::BackgroundLocalAgentFanoutProjection {
                    group_id: "review-group".into(),
                    group_title: "storage review".into(),
                    target_count: 1,
                    slot_index: 0,
                    slot_label: "correctness".into(),
                },
            ),
        };

        assert_eq!(
            spawner
                .restore_workspace_agent_projections(std::slice::from_ref(&projection))
                .await,
            1
        );
        assert!(matches!(
            spawner
                .wait_for_agent_outcome("reviewer@lost", std::time::Duration::ZERO)
                .await,
            WaitForAgentOutcome::Status(AgentStatus::Interrupted {
                partial_result,
                finish_reason,
            }) if partial_result == "partial finding"
                && finish_reason == "local_executor_unavailable_after_resume"
        ));
        let groups = spawner.list_fanout_groups().await;
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].group_id, "review-group");
        assert_eq!(groups[0].summary().interrupted, 1);
        assert_eq!(
            groups[0].slots[0].run_id.as_deref(),
            Some("run-reviewer-lost")
        );

        assert_eq!(
            spawner
                .restore_workspace_agent_projections(std::slice::from_ref(&projection))
                .await,
            0,
            "recovery must be idempotent"
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
