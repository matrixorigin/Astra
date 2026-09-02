//! Dynamic agent spawner — runtime agent creation and lifecycle management.

use crate::messaging::SubRunInfo;
use astra_messaging::router::AgentMailboxRouter;
use astra_messaging::types::{AgentAddress, AgentMessage, MessagePayload, MessageTarget};
use astra_turn_core::fork_prefix_store::PrefixCaptureSink;
use astra_turn_core::fork_reconstruct::reconstruct_messages;
use astra_turn_core::fork_resolve::{
    PrefixResolveOutcome, ResolveFailure, SpawnResolveContext, resolve_inherit_prefix,
};
use astra_turn_core::orchestration_context_cache::SharedContextCache;
use astra_turn_core::orchestration_fanout_group::{
    AgentFanoutGroupProjection, AgentFanoutSlotIdentity, AgentFanoutSlotStatus, AgentFanoutStatus,
};
use astra_turn_core::orchestration_progress::{
    AgentProgressEvent, ProgressBroadcaster, ProgressEventType,
};
use astra_turn_core::orchestration_spawn_tool::{SpawnAgentInput, SpawnAgentOutput};
use astra_turn_core::orchestration_types::CancellationOrigin;
use astra_turn_core::trace_event::{TraceContext, TraceEvent, TraceEventWriter};
use futures_util::{FutureExt, StreamExt};

use sha2::{Digest, Sha256};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
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
const AGENT_FINISH_REASON_DURABLE_RESULT_UNAVAILABLE: &str = "durable_result_unavailable";
pub const CANCELLATION_ORIGIN_UNVERIFIED: &str = "cancellation_origin_unverified";
/// Durable cancellation is best-effort control-plane reconciliation. It must
/// never keep local structured concurrency from reaching a terminal state.
const AGENT_DURABLE_CANCEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Persistence, mailbox, and worktree cleanup happen after the local terminal
/// projection on a runtime deadline. They are best-effort and must not reopen
/// the foreground group's wall-clock bound.
const AGENT_DEADLINE_CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const AGENT_TERMINAL_DELIVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
const AGENT_MAILBOX_UNREGISTER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
const AGENT_TRACE_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const AGENT_TERMINAL_JOURNAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
const CANCELLATION_RETRY_INITIAL_DELAY: std::time::Duration = std::time::Duration::from_millis(100);
const CANCELLATION_RETRY_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(5);
/// One session/root may occupy only a small part of the process-wide durable
/// cancellation budget. A large fanout therefore cannot enqueue all of its
/// database work ahead of a newly-cancelled session.
const CANCELLATION_RETRY_PER_SPAWNER_CONCURRENCY: usize = 4;
/// A database outage can leave cancellation debt in many independent
/// sessions. Keep retries fair within each spawner, but cap the aggregate
/// control-plane pressure process-wide so one outage cannot turn O(sessions)
/// supervisors into O(sessions) concurrent database requests.
const CANCELLATION_RETRY_GLOBAL_CONCURRENCY: usize = 32;
#[cfg(test)]
const TEST_SPAWN_PREPARATION_PERMITS: u32 = 1_024;

fn cancellation_retry_global_capacity() -> &'static Arc<tokio::sync::Semaphore> {
    static CAPACITY: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    CAPACITY.get_or_init(|| {
        Arc::new(tokio::sync::Semaphore::new(
            CANCELLATION_RETRY_GLOBAL_CONCURRENCY,
        ))
    })
}

fn effective_spawn_allowed_tools(
    requested: Option<&[String]>,
    profile_defaults: &HashSet<String>,
) -> Vec<String> {
    let profile_defaults = astra_turn_core::tool_allowlist::normalize_tool_names(profile_defaults);
    let profile_is_unrestricted = profile_defaults.contains("*");
    let requested = requested.map(astra_turn_core::tool_allowlist::normalize_tool_names);
    let mut tools: Vec<String> = match requested {
        None => profile_defaults.iter().cloned().collect::<Vec<_>>(),
        Some(requested) if profile_is_unrestricted => requested.into_iter().collect(),
        Some(requested) if requested.contains("*") => profile_defaults.iter().cloned().collect(),
        Some(requested) => requested.intersection(&profile_defaults).cloned().collect(),
    };
    if tools.iter().any(|tool| tool == "*") {
        return vec!["*".to_string()];
    }
    tools.sort();
    tools.dedup();
    tools
}

/// Stable cause and proven origin shared by descendant cancellation and
/// durable run cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DescendantCancellationReason {
    origin: CancellationOrigin,
}

impl DescendantCancellationReason {
    pub const fn ancestor_cancelled(origin: CancellationOrigin) -> Self {
        Self { origin }
    }

    pub const fn origin(self) -> CancellationOrigin {
        self.origin
    }

    pub const fn as_str(self) -> &'static str {
        match self.origin {
            CancellationOrigin::User => "user cancelled ancestor run before child completion",
            CancellationOrigin::Runtime => "runtime cancelled ancestor run before child completion",
            CancellationOrigin::Unverified => "ancestor cancellation origin could not be verified",
        }
    }
}

/// The producer-owned result of stopping one fixed-size fanout group.
///
/// Tool and UI control paths use this rather than independently reimplementing
/// slot selection and cancellation-race handling.
#[derive(Debug, Clone, PartialEq)]
pub struct FanoutGroupCancellation {
    pub group: AgentFanoutGroupProjection,
    /// Local execution ownership was seized, but no authoritative durable
    /// terminal (or superseding generation) had been observed at return.
    pub cancellation_pending_agent_ids: Vec<String>,
    pub stopped_agent_ids: Vec<String>,
    pub not_stopped_agent_ids: Vec<String>,
    pub already_terminal_count: usize,
    pub non_stoppable_count: usize,
}

/// Result of transferring one exact local execution into durable
/// cancellation reconciliation. This deliberately does not claim which
/// durable terminal will win.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancellationTransferOutcome {
    SeizedPending,
    AlreadyPending,
    AlreadyTerminal,
    NotFound,
}

impl CancellationTransferOutcome {
    pub const fn owns_local_stop(self) -> bool {
        matches!(self, Self::SeizedPending)
    }

    pub const fn is_pending(self) -> bool {
        matches!(self, Self::SeizedPending | Self::AlreadyPending)
    }
}

struct DurableCancellationAdmission {
    agent_id: String,
    run_id: String,
    owner_version: u64,
}

struct LocalCancellationTransfer {
    outcome: CancellationTransferOutcome,
    admission: Option<DurableCancellationAdmission>,
}

impl LocalCancellationTransfer {
    fn observed(outcome: CancellationTransferOutcome) -> Self {
        Self {
            outcome,
            admission: None,
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
        SpawnRunStatusKind::Cancelled => {
            let reason = run_result
                .error
                .as_deref()
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or(run_result.finish_reason.as_str());
            match run_result.cancellation_origin {
                CancellationOrigin::User => AgentStatus::cancelled_by_user(reason),
                CancellationOrigin::Runtime => AgentStatus::Cancelled {
                    by_user: false,
                    reason: reason.to_string(),
                },
                CancellationOrigin::Unverified => AgentStatus::Interrupted {
                    partial_result: run_result.output.clone().unwrap_or_default(),
                    finish_reason: CANCELLATION_ORIGIN_UNVERIFIED.to_string(),
                },
            }
        }
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
            } else if result.trim().is_empty() {
                AgentStatus::Interrupted {
                    partial_result: result,
                    finish_reason: AGENT_FINISH_REASON_DURABLE_RESULT_UNAVAILABLE.to_string(),
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

fn cancellation_agent_status(reason: &str, origin: CancellationOrigin) -> AgentStatus {
    match origin {
        CancellationOrigin::User => AgentStatus::cancelled_by_user(reason),
        CancellationOrigin::Runtime => AgentStatus::Cancelled {
            by_user: false,
            reason: reason.to_string(),
        },
        CancellationOrigin::Unverified => AgentStatus::Interrupted {
            partial_result: String::new(),
            finish_reason: CANCELLATION_ORIGIN_UNVERIFIED.to_string(),
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
            if run_result.cancellation_origin == CancellationOrigin::Unverified {
                return SpawnAgentOutput::Interrupted {
                    agent_id,
                    run_id,
                    result: run_result.output.unwrap_or_default(),
                    finish_reason: CANCELLATION_ORIGIN_UNVERIFIED.to_string(),
                    tool_calls: run_result.tool_calls,
                    duration_ms,
                };
            }
            let finish_reason = if run_result.finish_reason.trim().is_empty() {
                SPAWN_STATUS_CANCELLED.to_string()
            } else {
                run_result.finish_reason
            };
            let reason = run_result
                .error
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or_else(|| finish_reason.clone());
            SpawnAgentOutput::Cancelled {
                agent_id,
                run_id,
                reason,
                finish_reason,
                cancelled_by_user: run_result.cancellation_origin == CancellationOrigin::User,
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
                // Completion provenance belongs to the restored workspace
                // projection, not to outcome semantics. Keep the canonical
                // finish reason normal so downstream lifecycle consumers do
                // not reinterpret a successful restore as an interruption.
                finish_reason: Some(
                    astra_turn_core::orchestration_types::AGENT_FINISH_REASON_NORMAL.into(),
                ),
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
        astra_core::STATUS_COMPLETED
            | astra_core::STATUS_FAILED
            | astra_core::STATUS_CANCELLED
            | astra_core::STATUS_DELEGATED
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

pub(crate) fn durable_agent_status(run: &astra_services::runs::DurableRunRecord) -> AgentStatus {
    let output = durable_run_text(run).unwrap_or_default();
    match run.status.as_str() {
        astra_core::STATUS_COMPLETED if !output.is_empty() => AgentStatus::Completed {
            result: output,
            finish_reason: Some(
                astra_turn_core::orchestration_types::AGENT_FINISH_REASON_NORMAL.into(),
            ),
        },
        astra_core::STATUS_COMPLETED => AgentStatus::Interrupted {
            partial_result: String::new(),
            finish_reason: AGENT_FINISH_REASON_DURABLE_RESULT_UNAVAILABLE.into(),
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
        astra_core::STATUS_CANCELLED => {
            let terminal =
                run.events.iter().rev().find(|event| {
                    astra_services::runs::extract_event_type(event) == "run_finished"
                });
            let reason = terminal
                .and_then(|event| event.pointer("/data/reason"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("cancelled");
            match terminal
                .and_then(|event| event.pointer("/data/cancellation_origin"))
                .and_then(serde_json::Value::as_str)
                .and_then(|origin| origin.parse().ok())
                .unwrap_or(CancellationOrigin::Unverified)
            {
                CancellationOrigin::User => AgentStatus::cancelled_by_user(reason),
                CancellationOrigin::Runtime => AgentStatus::Cancelled {
                    by_user: false,
                    reason: reason.to_string(),
                },
                CancellationOrigin::Unverified => AgentStatus::Interrupted {
                    partial_result: output,
                    finish_reason: CANCELLATION_ORIGIN_UNVERIFIED.to_string(),
                },
            }
        }
        astra_core::STATUS_DELEGATED => AgentStatus::Interrupted {
            partial_result: output,
            finish_reason: astra_core::STATUS_DELEGATED.into(),
        },
        astra_core::STATUS_PAUSED => AgentStatus::Waiting {
            reason: run
                .waiting_for
                .clone()
                .unwrap_or_else(|| "durable child is paused and can be resumed".into()),
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

/// Merge independently durable lifecycle truth with a locally owned result
/// payload. A terminal row may become visible before its `text_done` event is
/// readable; that lower-fidelity snapshot must not erase a complete result
/// already returned by the executor. Different durable terminal states still
/// win, so cancellation/failure cannot be hidden by stale local success.
fn reconciled_durable_agent_status(
    current: &AgentStatus,
    run: &astra_services::runs::DurableRunRecord,
) -> AgentStatus {
    let durable = durable_agent_status(run);
    match (run.status.as_str(), current, &durable) {
        (
            astra_core::STATUS_COMPLETED,
            AgentStatus::Completed { result, .. },
            AgentStatus::Interrupted {
                partial_result,
                finish_reason,
            },
        ) if !result.trim().is_empty()
            && partial_result.is_empty()
            && finish_reason == AGENT_FINISH_REASON_DURABLE_RESULT_UNAVAILABLE =>
        {
            current.clone()
        }
        (
            astra_core::STATUS_FAILED,
            AgentStatus::Interrupted { partial_result, .. },
            AgentStatus::Interrupted {
                partial_result: durable_partial,
                finish_reason,
            },
        ) if !partial_result.trim().is_empty() && durable_partial.is_empty() => {
            AgentStatus::Interrupted {
                partial_result: partial_result.clone(),
                finish_reason: finish_reason.clone(),
            }
        }
        _ => durable,
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
            // AgentStatus is the typed lifecycle authority. A non-empty
            // finish_reason may describe provenance (for example a durable
            // restore) rather than an interruption. Treat only canonical
            // interruption reasons as interrupted; otherwise restoring a
            // completed child publishes a contradictory terminal event and
            // makes UI/audit projections regress from completed to stopped.
            if agent_completion_is_interrupted(finish_reason.as_deref()) {
                let reason = agent_finish_reason_text(finish_reason.as_deref());
                Some(ProgressEventType::Interrupted {
                    reason: reason.to_string(),
                    partial_summary: result.clone(),
                    total_tool_calls: metrics.tool_calls,
                    total_tokens: (metrics.prompt_tokens, metrics.completion_tokens),
                    duration_ms,
                })
            } else {
                Some(ProgressEventType::Completed {
                    result_summary: result.clone(),
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
            origin: if *by_user {
                CancellationOrigin::User
            } else {
                CancellationOrigin::Runtime
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
    /// Provider model name already resolved from the parent's admitted
    /// Offering. This is execution material, never a child policy choice.
    pub resolved_model_name: Option<String>,
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
    /// Parent admission's typed workspace-effect boundary.
    pub workspace_mutation: astra_config::user_profile::WorkspaceMutationIntent,
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
    /// Opaque capability bound to one concrete executor invocation. Runtime
    /// cancellation must present this identity instead of re-resolving mutable
    /// state by run_id and accidentally targeting a recovered generation.
    pub cancellation_binding_id: Option<String>,
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
    /// Opaque capability for this exact executor invocation.
    pub cancellation_binding_id: String,
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
    /// Initial adaptive execution slice selected by the agent persona and an
    /// optional complexity hint. This is a convergence checkpoint, not a hard
    /// stop unless `hard_turn_limit` is also present.
    pub initial_turns: u32,
    /// Caller-owned explicit hard limit. Persona defaults and qualitative
    /// complexity hints must not populate this field.
    pub hard_turn_limit: Option<u32>,
    /// Allowed tools for this agent type.
    pub allowed_tools: Vec<String>,
    /// Whether the agent is read-only.
    pub read_only: bool,
    /// Typed workspace-effect boundary inherited from the root admission.
    pub workspace_mutation: astra_config::user_profile::WorkspaceMutationIntent,
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
    /// Exact canonical WorkItem revision requested for this child. The server
    /// validates it against the parent's durable Work binding before insert.
    pub work_item: Option<astra_turn_core::orchestration_spawn_tool::WorkItemExecutionSpec>,
}

/// Durable acknowledgement returned by a spawned-run cancellation owner.
///
/// `Terminal` means the child run's authoritative terminal CAS is visible.
/// `RecoveryRecorded` means an exact, shared durable intent was committed but
/// terminalization is still pending. It does not retire the live retry owner.
#[derive(Debug, Clone, PartialEq)]
pub enum SpawnRunCancellationDurability {
    Terminal,
    RecoveryRecorded,
    /// A different durable terminal won the CAS. Cancellation is no longer
    /// pending; carry the exact winner so local projections cannot remain a
    /// stale cancellation when refresh is delayed or unavailable.
    Superseded(AgentStatus),
    /// The caller did not own the durable run's active generation. No
    /// cancellation fact was accepted; carry the authoritative non-terminal
    /// projection without pretending it is a terminal CAS winner.
    NotOwned(AgentStatus),
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
            .field("initial_turns", &self.initial_turns)
            .field("hard_turn_limit", &self.hard_turn_limit)
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
    /// Structured cancellation source. This is authoritative only when the
    /// run ended in the cancelled state; non-cancelled results use
    /// [`CancellationOrigin::Unverified`].
    pub cancellation_origin: CancellationOrigin,
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
        _cancellation_binding_id: Option<&str>,
        _user_id: Option<&str>,
        _reason: &str,
        _origin: CancellationOrigin,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Cancel a spawned run and report whether durable terminal state is
    /// already visible or an exact shared recovery intent owns convergence.
    /// Existing in-process executors inherit the terminal acknowledgement;
    /// server executors override this method to expose their durable outbox.
    async fn cancel_spawned_run_durably(
        &self,
        run_id: &str,
        cancellation_binding_id: Option<&str>,
        user_id: Option<&str>,
        reason: &str,
        origin: CancellationOrigin,
    ) -> Result<SpawnRunCancellationDurability, String> {
        self.cancel_spawned_run(run_id, cancellation_binding_id, user_id, reason, origin)
            .await?;
        Ok(SpawnRunCancellationDurability::Terminal)
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

#[derive(Clone, Debug)]
struct InFlightCancellation {
    origin: CancellationOrigin,
    reason: String,
    /// Immutable local-tree lineage retained across the active -> archive
    /// publication gap. `run_id` plus `cancellation_binding_id` identify the
    /// exact execution generation; neither may change when control authority
    /// is upgraded.
    run_id: String,
    parent_run_id: String,
    cancellation_binding_id: Option<String>,
    user_id: Option<String>,
    /// Monotonic control-owner generation. A stronger cancellation request
    /// replaces this token so an already-cloned weaker durable future is
    /// dropped before it can publish a stale result.
    owner_version: u64,
    owner_changed: tokio_util::sync::CancellationToken,
    /// Newly seized/upgraded owners are invisible to the retry supervisor
    /// until their whole local batch has transferred ownership.
    durable_ready: bool,
    /// Once a durable outcome wins the owner-version fence, later requests
    /// cannot rewrite that already-linearized terminal authority.
    finalizing: bool,
    retry_count: u32,
}

/// Min-heap entry ordered by the next permitted attempt and then FIFO
/// sequence. The exact run id makes stale entries harmless after a generation
/// is superseded.
type ScheduledCancellation = Reverse<(tokio::time::Instant, u64, String, String)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DurableCancellationAttempt {
    Settled,
    Pending,
    OwnerChanged,
    Shutdown,
}

const fn cancellation_origin_precedence(origin: CancellationOrigin) -> u8 {
    match origin {
        CancellationOrigin::Unverified => 0,
        CancellationOrigin::Runtime => 1,
        CancellationOrigin::User => 2,
    }
}

fn cancellation_retry_delay(retry_count: u32) -> std::time::Duration {
    let shift = retry_count.saturating_sub(1).min(16);
    (CANCELLATION_RETRY_INITIAL_DELAY * (1_u32 << shift)).min(CANCELLATION_RETRY_MAX_DELAY)
}

/// Owns a newly-created isolated worktree until active agent state has taken
/// cleanup responsibility. Spawn futures are cancellable at every await, so
/// an ordinary local variable is not enough to prevent pre-accept leaks.
struct PendingWorktreeCleanup {
    path: Option<PathBuf>,
    agent_id: String,
}

impl PendingWorktreeCleanup {
    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for PendingWorktreeCleanup {
    fn drop(&mut self) {
        cleanup_agent_worktree(self.path.as_ref(), &self.agent_id);
    }
}

struct LifecycleActivityGuard {
    epoch: Arc<std::sync::atomic::AtomicU64>,
    count: Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for LifecycleActivityGuard {
    fn drop(&mut self) {
        // Publish every mutation performed while the guard was held before
        // advertising quiescence to the server registry pruner.
        self.epoch.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.count.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
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
    /// Monotonic process-local witness for lifecycle activity that can make an
    /// otherwise idle spawner unsafe to evict. Server registry pruning takes a
    /// snapshot around its asynchronous idle checks and compares it again at
    /// the synchronous removal boundary.
    activity_epoch: Arc<std::sync::atomic::AtomicU64>,
    lifecycle_activity_count: Arc<std::sync::atomic::AtomicUsize>,
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
    /// Explicit test/local journal root captured at the session ownership
    /// boundary. Tokio blocking workers do not inherit thread-local guards.
    journal_dir_override: Arc<std::sync::RwLock<Option<PathBuf>>>,
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
    /// Short synchronous gate shared with final JoinSet installation. Long
    /// asynchronous spawn preparation never holds this lock; shutdown closes
    /// it immediately and cancels preparation through the token below.
    background_task_admission: Arc<std::sync::Mutex<bool>>,
    background_task_shutdown: tokio_util::sync::CancellationToken,
    #[cfg(test)]
    spawn_preparation_gate: Arc<tokio::sync::Semaphore>,
    #[cfg(test)]
    spawn_before_reservation_barriers:
        Arc<std::sync::Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>>,
    #[cfg(test)]
    cancellation_before_in_flight_hook:
        Arc<std::sync::Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>>,
    #[cfg(test)]
    descendant_snapshot_before_pending_hook:
        Arc<std::sync::Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>>,
    #[cfg(test)]
    shutdown_active_snapshot_count: Arc<std::sync::atomic::AtomicUsize>,
    /// Per-agent abort handles for background children so the parent can cancel
    /// a single lagging sub-agent without killing siblings.
    background_abort_handles: Arc<RwLock<HashMap<String, tokio::task::AbortHandle>>>,
    /// Cancellation ownership remains queryable while durable control is
    /// pending after active/archive state has been seized.
    in_flight_cancellations: Arc<RwLock<HashMap<String, InFlightCancellation>>>,
    /// At most one supervised retry loop per session spawner. Every retained
    /// marker is therefore backed by a live retry owner while the process is
    /// accepting work; shutdown cancels the loop and reports the marker as an
    /// unclean drain instead of silently forgetting it.
    cancellation_retry_supervisor_running: Arc<std::sync::atomic::AtomicBool>,
    /// FIFO of exact cancellation owners awaiting durable reconciliation.
    /// The authoritative jobs live in `in_flight_cancellations`; this queue is
    /// only an O(1) ready index, so the supervisor never scans every retained
    /// owner after an outage.
    cancellation_retry_queue: Arc<std::sync::Mutex<BinaryHeap<ScheduledCancellation>>>,
    cancellation_retry_sequence: Arc<std::sync::atomic::AtomicU64>,
    cancellation_retry_wakeup: Arc<tokio::sync::Notify>,
    #[cfg(test)]
    cancellation_capacity_override: Arc<std::sync::RwLock<Option<Arc<tokio::sync::Semaphore>>>>,
    #[cfg(test)]
    cancellation_capacity_waiting_hook: Arc<std::sync::Mutex<Option<Arc<tokio::sync::Notify>>>>,
    #[cfg(test)]
    cancellation_retry_panic_after_dequeue: Arc<std::sync::atomic::AtomicBool>,
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

/// Clears the single-supervisor fence on every exit path, including unwind,
/// and closes the queue-empty race before releasing its session ownership.
struct CancellationRetrySupervisorGuard {
    spawner: DynamicAgentSpawner,
}

impl Drop for CancellationRetrySupervisorGuard {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;

        self.spawner
            .cancellation_retry_supervisor_running
            .store(false, Ordering::Release);
        if !self.spawner.background_task_shutdown.is_cancelled()
            && !self
                .spawner
                .cancellation_retry_queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        {
            self.spawner.ensure_cancellation_retry_supervisor();
        }
    }
}

/// Owns every ready-index entry removed for one retry wave. An unwind may
/// drop some or all executor futures before they report an outcome; requeueing
/// the exact bindings keeps their authoritative in-flight owners reachable.
struct CancellationRetryBatchGuard {
    queue: Arc<std::sync::Mutex<BinaryHeap<ScheduledCancellation>>>,
    sequence: Arc<std::sync::atomic::AtomicU64>,
    wakeup: Arc<tokio::sync::Notify>,
    scheduled: Vec<(String, String)>,
    armed: bool,
}

impl CancellationRetryBatchGuard {
    fn new(spawner: &DynamicAgentSpawner, scheduled: Vec<(String, String)>) -> Self {
        Self {
            queue: Arc::clone(&spawner.cancellation_retry_queue),
            sequence: Arc::clone(&spawner.cancellation_retry_sequence),
            wakeup: Arc::clone(&spawner.cancellation_retry_wakeup),
            scheduled,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
        self.scheduled.clear();
    }

    fn disarm_binding(&mut self, agent_id: &str, run_id: &str) {
        if let Some(index) = self
            .scheduled
            .iter()
            .position(|(queued_agent_id, queued_run_id)| {
                queued_agent_id == agent_id && queued_run_id == run_id
            })
        {
            self.scheduled.swap_remove(index);
        }
    }
}

impl Drop for CancellationRetryBatchGuard {
    fn drop(&mut self) {
        if !self.armed || self.scheduled.is_empty() {
            return;
        }
        let ready_at = tokio::time::Instant::now();
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (agent_id, run_id) in self.scheduled.drain(..) {
            let sequence = self
                .sequence
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            queue.push(Reverse((ready_at, sequence, agent_id, run_id)));
        }
        drop(queue);
        self.wakeup.notify_one();
    }
}

impl DynamicAgentSpawner {
    /// Create a new spawner with the given dependencies.
    pub fn new(mailbox_router: Arc<AgentMailboxRouter>) -> Self {
        let background_task_owner = Arc::new(std::sync::Mutex::new(tokio::task::JoinSet::new()));
        Self {
            mailbox_router,
            active_agents: Arc::new(RwLock::new(HashMap::new())),
            activity_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            lifecycle_activity_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            cancelling_parent_runs: Arc::new(RwLock::new(HashSet::new())),
            progress_broadcaster: Arc::new(ProgressBroadcaster::default()),
            context_cache: Arc::new(SharedContextCache::default()),
            executor: None,
            session_id: Arc::new(std::sync::RwLock::new(None)),
            journal_dir_override: Arc::new(std::sync::RwLock::new(
                astra_services::session_journal::current_journal_dir_override(),
            )),
            agent_registry:
                astra_turn_core::orchestration_team_config::AgentRegistry::builtins_only(),
            completed_agents: Arc::new(RwLock::new(VecDeque::new())),
            background_tasks: Arc::downgrade(&background_task_owner),
            _background_task_owner: Some(background_task_owner),
            background_task_admission: Arc::new(std::sync::Mutex::new(true)),
            background_task_shutdown: tokio_util::sync::CancellationToken::new(),
            #[cfg(test)]
            spawn_preparation_gate: Arc::new(tokio::sync::Semaphore::new(
                TEST_SPAWN_PREPARATION_PERMITS as usize,
            )),
            #[cfg(test)]
            spawn_before_reservation_barriers: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            cancellation_before_in_flight_hook: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            descendant_snapshot_before_pending_hook: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            shutdown_active_snapshot_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            background_abort_handles: Arc::new(RwLock::new(HashMap::new())),
            in_flight_cancellations: Arc::new(RwLock::new(HashMap::new())),
            cancellation_retry_supervisor_running: Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
            cancellation_retry_queue: Arc::new(std::sync::Mutex::new(BinaryHeap::new())),
            cancellation_retry_sequence: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            cancellation_retry_wakeup: Arc::new(tokio::sync::Notify::new()),
            #[cfg(test)]
            cancellation_capacity_override: Arc::new(std::sync::RwLock::new(None)),
            #[cfg(test)]
            cancellation_capacity_waiting_hook: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            cancellation_retry_panic_after_dequeue: Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
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

    fn begin_lifecycle_activity(&self) -> LifecycleActivityGuard {
        self.lifecycle_activity_count
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.activity_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        LifecycleActivityGuard {
            epoch: Arc::clone(&self.activity_epoch),
            count: Arc::clone(&self.lifecycle_activity_count),
        }
    }

    /// Process-local lifecycle activity witness used by the Server spawner
    /// registry's lock-free asynchronous idle inspection.
    pub(crate) fn activity_epoch(&self) -> u64 {
        self.activity_epoch
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn has_lifecycle_activity(&self) -> bool {
        self.lifecycle_activity_count
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
    }

    #[cfg(test)]
    pub(crate) fn set_cancellation_before_in_flight_hook(
        &self,
        hook: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    ) {
        *self
            .cancellation_before_in_flight_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = hook;
    }

    #[cfg(test)]
    fn set_descendant_snapshot_before_pending_hook(
        &self,
        hook: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    ) {
        *self
            .descendant_snapshot_before_pending_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = hook;
    }

    #[cfg(test)]
    fn set_cancellation_capacity_for_test(&self, capacity: Arc<tokio::sync::Semaphore>) {
        *self
            .cancellation_capacity_override
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(capacity);
    }

    #[cfg(test)]
    fn set_cancellation_capacity_waiting_hook_for_test(
        &self,
        waiting: Option<Arc<tokio::sync::Notify>>,
    ) {
        *self
            .cancellation_capacity_waiting_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = waiting;
    }

    fn cancellation_capacity(&self) -> Arc<tokio::sync::Semaphore> {
        #[cfg(test)]
        if let Some(capacity) = self
            .cancellation_capacity_override
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return capacity;
        }
        Arc::clone(cancellation_retry_global_capacity())
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
                // Locally executing children remain owned by their executor
                // and are intentionally absent from this archived collection.
                // Once a local child has yielded a non-terminal archived
                // projection, however, an ancestor or another pod can move
                // its durable run to a terminal state. Terminal durable truth
                // is monotonic and must settle that stale local observation
                // even when the child was not originally restored as remote.
                if !observed.contains(agent_id) && !durable_run_is_terminal(&run.status) {
                    continue;
                }
                let Some(state) = completed
                    .iter_mut()
                    .find(|state| state.agent_id == agent_id)
                else {
                    continue;
                };
                let status = reconciled_durable_agent_status(&state.status, run);
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

    fn ensure_cancellation_retry_supervisor(&self) {
        use std::sync::atomic::Ordering;

        if self.background_task_shutdown.is_cancelled()
            || self
                .cancellation_retry_supervisor_running
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        let spawner = self.clone_for_task();
        tokio::spawn(async move {
            let supervisor = CancellationRetrySupervisorGuard { spawner };
            let spawner = &supervisor.spawner;
            loop {
                if spawner.background_task_shutdown.is_cancelled() {
                    break;
                }

                // A min-heap gives every exact owner its own backoff deadline.
                // New jobs wake this supervisor and can run immediately, but
                // cannot cause an older failed job to retry before `ready_at`.
                let (scheduled, next_ready_at) = {
                    let mut queue = spawner
                        .cancellation_retry_queue
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let now = tokio::time::Instant::now();
                    let mut scheduled =
                        Vec::with_capacity(CANCELLATION_RETRY_PER_SPAWNER_CONCURRENCY);
                    while scheduled.len() < CANCELLATION_RETRY_PER_SPAWNER_CONCURRENCY {
                        let Some(Reverse((ready_at, _, _, _))) = queue.peek() else {
                            break;
                        };
                        if *ready_at > now {
                            break;
                        }
                        let Reverse((_, _, agent_id, run_id)) = queue
                            .pop()
                            .expect("peeked durable cancellation must remain queued");
                        if scheduled.iter().any(|(queued_agent_id, queued_run_id)| {
                            queued_agent_id == &agent_id && queued_run_id == &run_id
                        }) {
                            // One ready-index entry is sufficient for an exact
                            // binding. A prior unwind may conservatively have
                            // left a duplicate; never execute it concurrently.
                            continue;
                        }
                        scheduled.push((agent_id, run_id));
                    }
                    let next_ready_at = queue.peek().map(|Reverse((ready_at, _, _, _))| *ready_at);
                    (scheduled, next_ready_at)
                };
                if scheduled.is_empty() {
                    let Some(next_ready_at) = next_ready_at else {
                        break;
                    };
                    tokio::select! {
                        _ = spawner.background_task_shutdown.cancelled() => break,
                        _ = spawner.cancellation_retry_wakeup.notified() => {}
                        _ = tokio::time::sleep_until(next_ready_at) => {}
                    }
                    continue;
                }
                let mut scheduled = CancellationRetryBatchGuard::new(spawner, scheduled);
                #[cfg(test)]
                if spawner
                    .cancellation_retry_panic_after_dequeue
                    .swap(false, Ordering::AcqRel)
                {
                    panic!("injected durable cancellation panic after dequeue");
                }

                let jobs = {
                    let pending = spawner.in_flight_cancellations.read().await;
                    scheduled
                        .scheduled
                        .iter()
                        .filter_map(|(agent_id, run_id)| {
                            pending.get(agent_id.as_str()).and_then(|job| {
                                (job.run_id == run_id.as_str() && job.durable_ready)
                                    .then(|| (agent_id.clone(), job.clone()))
                            })
                        })
                        .collect::<Vec<_>>()
                };
                if jobs.is_empty() {
                    scheduled.disarm();
                    tokio::task::yield_now().await;
                    continue;
                }

                let outcomes =
                    futures_util::stream::iter(jobs.into_iter().map(|(agent_id, job)| {
                        let spawner = spawner.clone_for_task();
                        async move {
                            let outcome = spawner
                                .retry_in_flight_cancellation(&agent_id, job.clone())
                                .await;
                            (agent_id, job.run_id, job.owner_version, outcome)
                        }
                    }))
                    .buffer_unordered(CANCELLATION_RETRY_PER_SPAWNER_CONCURRENCY)
                    .collect::<Vec<_>>()
                    .await;

                let mut shutdown = false;
                for (agent_id, run_id, owner_version, outcome) in outcomes {
                    match outcome {
                        DurableCancellationAttempt::Settled => {}
                        DurableCancellationAttempt::Pending => {
                            let retry_count = {
                                let mut pending = spawner.in_flight_cancellations.write().await;
                                let Some(job) = pending.get_mut(&agent_id).filter(|job| {
                                    job.run_id == run_id
                                        && job.owner_version == owner_version
                                        && job.durable_ready
                                }) else {
                                    continue;
                                };
                                job.retry_count = job.retry_count.saturating_add(1);
                                job.retry_count
                            };
                            spawner.schedule_durable_cancellation(
                                agent_id.clone(),
                                run_id.clone(),
                                tokio::time::Instant::now() + cancellation_retry_delay(retry_count),
                                false,
                            );
                        }
                        DurableCancellationAttempt::OwnerChanged => {
                            // The stronger owner admitted its replacement job
                            // after the full local batch transfer. Never
                            // requeue this stale version.
                        }
                        DurableCancellationAttempt::Shutdown => {
                            // Preserve the explicit owner while the spawner is
                            // still queryable, but do not let this detached task
                            // retain the executor/session after root shutdown.
                            spawner.schedule_durable_cancellation(
                                agent_id.clone(),
                                run_id.clone(),
                                tokio::time::Instant::now(),
                                false,
                            );
                            shutdown = true;
                        }
                    }
                    scheduled.disarm_binding(&agent_id, &run_id);
                }
                scheduled.disarm();
                if shutdown {
                    break;
                }
                tokio::task::yield_now().await;
            }
        });
    }

    fn schedule_durable_cancellation(
        &self,
        agent_id: String,
        run_id: String,
        ready_at: tokio::time::Instant,
        wake: bool,
    ) {
        let sequence = self
            .cancellation_retry_sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.cancellation_retry_queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Reverse((ready_at, sequence, agent_id, run_id)));
        if wake {
            self.cancellation_retry_wakeup.notify_one();
        }
    }

    async fn retry_in_flight_cancellation(
        &self,
        agent_id: &str,
        job: InFlightCancellation,
    ) -> DurableCancellationAttempt {
        let owner_changed = job.owner_changed.clone();
        let capacity = self.cancellation_capacity();
        #[cfg(test)]
        let _global_capacity = if let Some(waiting) = self
            .cancellation_capacity_waiting_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            // Poll once so the test observes the real semaphore-contention
            // boundary, not a proxy such as task scheduling or queue dequeue.
            let acquire = capacity.acquire_owned();
            tokio::pin!(acquire);
            let first_poll =
                std::future::poll_fn(|cx| match std::future::Future::poll(acquire.as_mut(), cx) {
                    std::task::Poll::Ready(result) => std::task::Poll::Ready(Some(result)),
                    std::task::Poll::Pending => {
                        waiting.notify_one();
                        std::task::Poll::Ready(None)
                    }
                })
                .await;
            match first_poll {
                Some(permit) => {
                    permit.expect("process-wide cancellation retry capacity must remain open")
                }
                None => tokio::select! {
                    _ = self.background_task_shutdown.cancelled() => {
                        return DurableCancellationAttempt::Shutdown;
                    }
                    _ = owner_changed.cancelled() => {
                        return DurableCancellationAttempt::OwnerChanged;
                    }
                    permit = &mut acquire => {
                        permit.expect("process-wide cancellation retry capacity must remain open")
                    }
                },
            }
        } else {
            tokio::select! {
                _ = self.background_task_shutdown.cancelled() => {
                    return DurableCancellationAttempt::Shutdown;
                }
                _ = owner_changed.cancelled() => {
                    return DurableCancellationAttempt::OwnerChanged;
                }
                permit = capacity.acquire_owned() => {
                    permit.expect("process-wide cancellation retry capacity must remain open")
                }
            }
        };
        #[cfg(not(test))]
        let _global_capacity = tokio::select! {
            _ = self.background_task_shutdown.cancelled() => {
                return DurableCancellationAttempt::Shutdown;
            }
            _ = owner_changed.cancelled() => {
                return DurableCancellationAttempt::OwnerChanged;
            }
            permit = capacity.acquire_owned() => {
                permit.expect("process-wide cancellation retry capacity must remain open")
            }
        };
        let outcome = if let Some(executor) = self.executor.as_ref() {
            tokio::select! {
                _ = self.background_task_shutdown.cancelled() => {
                    return DurableCancellationAttempt::Shutdown;
                }
                _ = owner_changed.cancelled() => {
                    return DurableCancellationAttempt::OwnerChanged;
                }
                outcome = tokio::time::timeout(
                    AGENT_DURABLE_CANCEL_TIMEOUT,
                    executor.cancel_spawned_run_durably(
                        &job.run_id,
                        job.cancellation_binding_id.as_deref(),
                        job.user_id.as_deref(),
                        &job.reason,
                        job.origin,
                    ),
                ) => match outcome {
                    Ok(outcome) => outcome,
                    Err(_) => Err(format!(
                        "executor cancellation exceeded {}ms",
                        AGENT_DURABLE_CANCEL_TIMEOUT.as_millis()
                    )),
                }
            }
        } else {
            Ok(SpawnRunCancellationDurability::Terminal)
        };

        // `select!` may observe a ready executor result at the same instant as
        // an owner upgrade. The map generation is the linearization point;
        // never interpret or publish a result cloned by an older owner.
        if !self.cancellation_owner_is_current(agent_id, &job).await {
            return DurableCancellationAttempt::OwnerChanged;
        }

        let (settled, authoritative_status) = match outcome {
            Ok(SpawnRunCancellationDurability::Terminal) => {
                let status = cancellation_agent_status(&job.reason, job.origin);
                (true, Some(status))
            }
            Ok(SpawnRunCancellationDurability::Superseded(status)) if status.is_terminal() => {
                (true, Some(status))
            }
            Ok(SpawnRunCancellationDurability::NotOwned(status)) if !status.is_terminal() => {
                (true, Some(status))
            }
            Ok(SpawnRunCancellationDurability::Superseded(status)) => {
                tracing::error!(
                    target: "fanout",
                    %agent_id,
                    run_id = %job.run_id,
                    status = %Self::agent_status_trace_label(&status),
                    "durable cancellation protocol violation: Superseded must be terminal"
                );
                (false, None)
            }
            Ok(SpawnRunCancellationDurability::NotOwned(status)) => {
                tracing::error!(
                    target: "fanout",
                    %agent_id,
                    run_id = %job.run_id,
                    status = %Self::agent_status_trace_label(&status),
                    "durable cancellation protocol violation: NotOwned must be non-terminal"
                );
                (false, None)
            }
            Ok(SpawnRunCancellationDurability::RecoveryRecorded) => (false, None),
            Err(error) => {
                tracing::warn!(
                    target: "fanout",
                    %agent_id,
                    run_id = %job.run_id,
                    %error,
                    "durable child cancellation retry remains pending"
                );
                (false, None)
            }
        };
        if let Some(status) = authoritative_status {
            // Do not claim terminal publication before the seized projection
            // exists. This is the active-remove -> archive handoff window;
            // its exact owner remains visible to lineage traversal and retry.
            let projection_ready = self
                .completed_agents
                .read()
                .await
                .iter()
                .rev()
                .any(|state| state.agent_id == agent_id && state.run_id == job.run_id);
            if !projection_ready {
                return if self.cancellation_owner_is_current(agent_id, &job).await {
                    DurableCancellationAttempt::Pending
                } else {
                    DurableCancellationAttempt::OwnerChanged
                };
            }
            {
                let mut pending = self.in_flight_cancellations.write().await;
                let Some(current) = pending.get_mut(agent_id).filter(|current| {
                    current.run_id == job.run_id
                        && current.owner_version == job.owner_version
                        && current.durable_ready
                        && !current.finalizing
                }) else {
                    return DurableCancellationAttempt::OwnerChanged;
                };
                current.finalizing = true;
            }
            if !self
                .apply_authoritative_agent_status(agent_id, &job.run_id, status)
                .await
            {
                // The primary cancellation owner may still hold the seized
                // state before publishing its local archive. Keep the marker
                // and retry owner alive; otherwise a fast durable winner can
                // be forgotten and then overwritten by the late provisional
                // cancellation projection.
                let mut pending = self.in_flight_cancellations.write().await;
                if let Some(current) = pending.get_mut(agent_id).filter(|current| {
                    current.run_id == job.run_id && current.owner_version == job.owner_version
                }) {
                    current.finalizing = false;
                }
                return DurableCancellationAttempt::Pending;
            }
        }
        if settled {
            let mut pending = self.in_flight_cancellations.write().await;
            if pending.get(agent_id).is_some_and(|current| {
                current.run_id == job.run_id && current.owner_version == job.owner_version
            }) {
                pending.remove(agent_id);
            }
        }
        if settled {
            DurableCancellationAttempt::Settled
        } else {
            DurableCancellationAttempt::Pending
        }
    }

    async fn cancellation_owner_is_current(
        &self,
        agent_id: &str,
        job: &InFlightCancellation,
    ) -> bool {
        self.in_flight_cancellations
            .read()
            .await
            .get(agent_id)
            .is_some_and(|current| {
                current.run_id == job.run_id
                    && current.owner_version == job.owner_version
                    && current.durable_ready
                    && !current.finalizing
            })
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
        let _activity = self.begin_lifecycle_activity();
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
                cancellation_binding_id: None,
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
        let _activity = self.begin_lifecycle_activity();
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
                cancellation_binding_id: None,
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

    /// Read one fanout group without cloning and sorting the entire bounded
    /// registry. Control paths call this by canonical group identity; full
    /// snapshots remain reserved for UI/telemetry enumeration.
    pub async fn fanout_group(&self, group_id: &str) -> Option<AgentFanoutGroupProjection> {
        self.fanout_groups.read().await.get(group_id).cloned()
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
        let _activity = self.begin_lifecycle_activity();
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

    /// Apply the exact durable status after cancellation reconciliation. The
    /// initial cancellation path keeps fanout non-terminal until this fact is
    /// known, so this is always the first irreversible group transition.
    async fn apply_authoritative_agent_status(
        &self,
        agent_id: &str,
        run_id: &str,
        status: AgentStatus,
    ) -> bool {
        let mut state = {
            let mut completed = self.completed_agents.write().await;
            if completed.iter().rev().any(|state| {
                state.agent_id == agent_id
                    && state.run_id == run_id
                    && state.status.is_terminal()
                    && state.status == status
            }) {
                return true;
            }
            let Some(position) = completed.iter().rposition(|state| {
                state.agent_id == agent_id && state.run_id == run_id && !state.status.is_terminal()
            }) else {
                return false;
            };
            completed
                .remove(position)
                .expect("located archived cancellation projection must remain present")
        };

        if status.is_terminal() {
            self.finalize_authoritative_seized_agent(&mut state, agent_id, status)
                .await
        } else {
            self.release_seized_agent_projection(&mut state, agent_id, status)
                .await
        }
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
        if let Some(dir) = astra_services::session_journal::current_journal_dir_override() {
            *self
                .journal_dir_override
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(dir);
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
        // Trace persistence is observational. It must not sit between an
        // accepted child state and installation of its cancellation handle,
        // or a slow database can make the child impossible for a group
        // deadline to seize. Keep the write bounded and off the lifecycle
        // critical path.
        let writer = Arc::clone(writer);
        tokio::spawn(async move {
            match tokio::time::timeout(AGENT_TRACE_WRITE_TIMEOUT, writer.write(event)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => astra_core::agent_error!(
                    "spawner",
                    "failed to persist agent lifecycle trace: {error}"
                ),
                Err(_) => astra_core::agent_warn!(
                    "spawner",
                    "agent lifecycle trace write exceeded {}ms",
                    AGENT_TRACE_WRITE_TIMEOUT.as_millis()
                ),
            }
        });
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

    #[cfg(test)]
    pub(crate) async fn hold_spawn_admission_for_test(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.spawn_preparation_gate
            .clone()
            .acquire_many_owned(TEST_SPAWN_PREPARATION_PERMITS)
            .await
            .expect("test spawn preparation gate remains open")
    }

    #[cfg(test)]
    pub(crate) async fn hold_fanout_projection_for_test(
        &self,
    ) -> tokio::sync::OwnedRwLockWriteGuard<HashMap<String, AgentFanoutGroupProjection>> {
        self.fanout_groups.clone().write_owned().await
    }

    #[cfg(test)]
    pub(crate) async fn has_in_flight_cancellation_for_test(&self, agent_id: &str) -> bool {
        self.in_flight_cancellations
            .read()
            .await
            .contains_key(agent_id)
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
        let _activity = self.begin_lifecycle_activity();
        if !*self
            .background_task_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            return Err(SpawnError::LifecycleShuttingDown);
        }
        let shutdown = self.background_task_shutdown.clone();
        let preparation_installed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let prepared = self.prepare_and_spawn(input, context, Arc::clone(&preparation_installed));
        tokio::pin!(prepared);
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                if preparation_installed.load(std::sync::atomic::Ordering::Acquire) {
                    prepared.await
                } else {
                    Err(SpawnError::LifecycleShuttingDown)
                }
            },
            result = &mut prepared => result,
        }
    }

    async fn prepare_and_spawn(
        &self,
        input: SpawnAgentInput,
        context: &SpawnContext,
        preparation_installed: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<SpawnAgentOutput, SpawnError> {
        #[cfg(test)]
        let _preparation_permit = self
            .spawn_preparation_gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| SpawnError::LifecycleShuttingDown)?;
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
        let effective_allowed_tools =
            effective_spawn_allowed_tools(input.allowed_tools.as_deref(), &agent_def.allowed_tools);
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
        let cancellation_binding_id = Uuid::new_v4().to_string();
        let agent_id = format!("{}@{}", agent_name, run_id);

        // 3. Determine model and turns
        let model = context.resolved_model_name.clone();
        // Budget resolution composes numeric and complexity ceilings by
        // taking the smaller value; with only one constraint, that constraint
        // is authoritative. See `resolve_turn_budget`.
        let initial_turns = astra_turn_core::orchestration_spawn_tool::resolve_turn_budget(
            input.max_turns,
            input.complexity.as_deref(),
            agent_def.max_turns,
        );
        // Preserve provenance: only an explicit numeric caller limit is a
        // hard boundary. Agent-type defaults and qualitative complexity are
        // initial scheduling slices that may renew while observed work keeps
        // making progress.
        let hard_turn_limit = input.max_turns.map(|turns| turns.max(1));
        // 3b. Resolve fork-prefix inheritance before any side effects
        // (mailbox, worktree, active_agents state). A hard-fail from
        // `required=true` must NOT leave half-constructed state
        // behind; soft-fallback recording only happens for spawns
        // that are definitely going to succeed.
        //
        // The resolver itself is a pure function over a store; if no
        // store is configured we skip even building the context
        // (saves a clone + RwLock write in the common path).
        let resolve_outcome = match (
            input.inherit_prefix.as_ref(),
            self.prefix_store.as_ref(),
            model.as_ref(),
        ) {
            (None, _, _) => PrefixResolveOutcome::Disabled,
            (Some(spec), Some(store), Some(model)) => {
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
                resolve_inherit_prefix(Some(spec), &resolve_ctx, store.as_ref())
            }
            (Some(spec), store, model) => {
                let reason = ResolveFailure::Unavailable {
                    reason: match (store.is_some(), model.is_some()) {
                        (false, false) => "prefix store and resolved child model are unavailable",
                        (false, true) => "prefix store is unavailable",
                        (true, false) => "resolved child model is unavailable",
                        (true, true) => unreachable!("complete resolver inputs matched above"),
                    }
                    .to_string(),
                };
                if spec.required {
                    PrefixResolveOutcome::Failed { reason }
                } else {
                    PrefixResolveOutcome::Fallback { reason }
                }
            }
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
            cancellation_binding_id: Some(cancellation_binding_id.clone()),
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
        #[cfg(test)]
        if let Some((entered, release)) = self
            .spawn_before_reservation_barriers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            // Deliberately synchronous: the regression must hold one
            // `prepare_and_spawn` poll in the exact window after the outer
            // select checked cancellation but before the first transferable
            // lifecycle owner is installed.
            entered.wait();
            release.wait();
        }
        if let Some(identity) = fanout_slot.as_ref()
            && self
                .fanout_group(&identity.group_id)
                .await
                .is_some_and(|group| group.is_terminal())
        {
            return Err(SpawnError::Race(format!(
                "fanout group '{}' settled before child reservation",
                identity.group_id
            )));
        }
        let capacity_rejection = {
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
            // This is the first side-effectful lifecycle boundary. Serialize
            // it with shutdown admission close so either the active-state owner
            // exists before shutdown snapshots, or no mailbox/worktree/durable
            // preparation can begin at all.
            let admission = self
                .background_task_admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !*admission {
                drop(admission);
                drop(active_agents);
                drop(cancellation_fence);
                return Err(SpawnError::LifecycleShuttingDown);
            }
            let capacity_rejection = self.max_concurrent_agents.and_then(|limit| {
                let active = active_agents.len();
                (active >= limit).then_some((active, limit))
            });
            if capacity_rejection.is_none() {
                active_agents.insert(agent_id.clone(), state);
            }
            drop(admission);
            drop(active_agents);
            drop(cancellation_fence);
            capacity_rejection
        };
        if let Some((active, limit)) = capacity_rejection {
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
        // Publish cleanup ownership before any further await. If the fanout
        // deadline drops this spawn while run-depth bookkeeping is stalled,
        // cancellation can now unregister the mailbox from active state.
        if let Some(address) = messaging_address.as_ref() {
            let published = {
                let mut active_agents = self.active_agents.write().await;
                active_agents.get_mut(&agent_id).is_some_and(|state| {
                    state.messaging_address = Some(address.clone());
                    true
                })
            };
            if !published {
                let _ = self.mailbox_router.unregister(address).await;
                self.record_fanout_spawn_rejected_for_input(
                    fanout_slot.as_ref(),
                    &input,
                    context,
                    format!("agent {agent_id} was cancelled during mailbox registration"),
                )
                .await;
                return Err(SpawnError::Race(format!(
                    "agent {agent_id} was cancelled during mailbox registration"
                )));
            }
        }
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
        let mut pending_worktree_cleanup = PendingWorktreeCleanup {
            path: worktree_path.clone(),
            agent_id: agent_id.clone(),
        };

        let spawned_state_for_trace = {
            let mut active_agents = self.active_agents.write().await;
            active_agents.get_mut(&agent_id).map(|state| {
                state.messaging_address = messaging_address.clone();
                state.worktree_path = worktree_path.clone();
                state.clone()
            })
        };
        let Some(spawned_state_for_trace) = spawned_state_for_trace else {
            if let Some(addr) = messaging_address.as_ref() {
                let _ = self.mailbox_router.unregister(addr).await;
            }
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
        };
        // Active state now owns both mailbox and worktree cleanup.
        pending_worktree_cleanup.disarm();
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
        // The effective per-spawn `allowed_tools` is intersected with
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
            .constrain_allowed_tools(effective_allowed_tools.iter().cloned());
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
        let workspace_mutation = if agent_def.read_only {
            astra_config::user_profile::WorkspaceMutationIntent::ReadOnly
        } else {
            context.workspace_mutation
        };
        let run_config = SpawnRunConfig {
            run_id: run_id.clone(),
            cancellation_binding_id,
            agent_id: agent_id.clone(),
            spawn_tool_call_id: context.spawn_tool_call_id.clone(),
            recursion_depth: child_recursion_depth,
            agent_type: input.agent_type.clone(),
            description: input.description.clone(),
            task: input.prompt.clone(),
            system_prompt_addendum: coordination_addendum,
            model,
            initial_turns,
            hard_turn_limit,
            allowed_tools: effective_allowed_tools,
            read_only: workspace_mutation
                == astra_config::user_profile::WorkspaceMutationIntent::ReadOnly,
            workspace_mutation,
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
            work_item: input.work_item.clone(),
        };

        // Emit agent_spawned journal event for unified timeline.
        if let Some(sid) = self.current_session_id() {
            let fanout_slot = fanout_slot
                .as_ref()
                .and_then(|slot| serde_json::to_value(slot).ok());
            let evt = astra_services::session_journal::JournalEvent::agent_spawned_with_fanout(
                Some(&sid),
                &agent_id,
                &run_id,
                &context.parent_run_id,
                &run_config.agent_type,
                &input.description,
                run_config.model.as_deref(),
                run_config.inherited_prefix.is_some(),
                fanout_slot.as_ref(),
                run_config.execution_metadata.as_ref(),
            );
            let writer = match context.trace_context.as_ref() {
                Some(trace) => {
                    astra_services::session_journal::JournalWriter::for_user(&trace.user_id, &sid)
                }
                None => astra_services::session_journal::JournalWriter::new(&sid),
            };
            if let Ok(writer) = writer {
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
            let _ = self
                .cancel_agent_with_origin(
                    &agent_id,
                    "agent lifecycle owner disappeared before execution",
                    CancellationOrigin::Runtime,
                )
                .await;
            return Err(SpawnError::LifecycleShuttingDown);
        };
        // Install execution ownership atomically with the final active-state
        // check. System cancellation takes the same handles -> active lock
        // order, so it either seizes the pre-handle reservation first (and no
        // executor starts) or observes this handle and aborts the child.
        let mut handles = self.background_abort_handles.write().await;
        if !self.active_agents.read().await.contains_key(&agent_id) {
            drop(handles);
            return Err(SpawnError::Race(format!(
                "agent {agent_id} was cancelled before executor ownership was installed"
            )));
        }
        let abort_handle = {
            let admission = self
                .background_task_admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *admission {
                Some(
                    background_tasks
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .spawn(spawn_future),
                )
            } else {
                None
            }
        };
        let Some(abort_handle) = abort_handle else {
            drop(handles);
            let _ = self
                .cancel_agent_with_origin(
                    &agent_id,
                    "agent lifecycle shut down before executor ownership was installed",
                    CancellationOrigin::Runtime,
                )
                .await;
            return Err(SpawnError::LifecycleShuttingDown);
        };
        handles.insert(agent_id.clone(), abort_handle);
        preparation_installed.store(true, std::sync::atomic::Ordering::Release);
        drop(handles);

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

    /// Transfer a single exact background execution into user-origin durable
    /// reconciliation without claiming its eventual terminal winner.
    pub async fn cancel_agent_for_user(
        &self,
        agent_id: &str,
        reason: &str,
    ) -> CancellationTransferOutcome {
        // A user stops an agent work subtree, not just one executor task. Own
        // the fence/snapshot in a detached task so dropping the HTTP/tool
        // caller cannot abort the target finalizer and leave descendants
        // executing without a parent.
        let spawner = self.clone_for_task();
        let agent_id = agent_id.to_string();
        let reason = reason.to_string();
        let owner = tokio::spawn(async move {
            let _activity = spawner.begin_lifecycle_activity();
            spawner
                .cancel_user_agent_subtree_owned(&agent_id, &reason)
                .await
        });
        owner.await.unwrap_or(CancellationTransferOutcome::NotFound)
    }

    /// Cancel a child as a parent-agent orchestration decision. Unlike direct
    /// UI control, this authority is runtime-owned and remains fenced to the
    /// exact child execution generation by the durable executor handoff.
    pub async fn cancel_agent_for_runtime(
        &self,
        agent_id: &str,
        reason: &str,
    ) -> CancellationTransferOutcome {
        self.cancel_agent_with_origin(agent_id, reason, CancellationOrigin::Runtime)
            .await
    }

    /// Fail-closed cancellation used when durable control cannot prove user
    /// or runtime origin. It remains exact-agent scoped and never acquires
    /// run-level descendant authority.
    pub async fn cancel_agent_for_unverified_runtime(
        &self,
        agent_id: &str,
        reason: &str,
    ) -> CancellationTransferOutcome {
        self.cancel_agent_with_origin(agent_id, reason, CancellationOrigin::Unverified)
            .await
    }

    /// Stop every currently stoppable slot in a fanout group as one
    /// producer-owned operation. The group id is a task-list identity, not an
    /// individual agent id.
    pub async fn cancel_fanout_group_for_user(
        &self,
        group_id: &str,
        reason: &str,
    ) -> Option<FanoutGroupCancellation> {
        self.cancel_fanout_group_with_origin(group_id, reason, CancellationOrigin::User)
            .await
    }

    /// Runtime-owned counterpart used by model/tool orchestration. Public UI
    /// callers intentionally continue to use the user-origin API above.
    pub(crate) async fn cancel_fanout_group_for_runtime(
        &self,
        group_id: &str,
        reason: &str,
    ) -> Option<FanoutGroupCancellation> {
        self.cancel_fanout_group_with_origin(group_id, reason, CancellationOrigin::Runtime)
            .await
    }

    /// Settle a foreground fanout whose runtime-owned wall-clock deadline
    /// elapsed. This is a system lifecycle boundary, not user cancellation.
    pub(crate) async fn cancel_fanout_group_for_deadline(
        &self,
        group_id: &str,
        reason: &str,
    ) -> Option<FanoutGroupCancellation> {
        self.cancel_fanout_group_with_origin(group_id, reason, CancellationOrigin::Runtime)
            .await
    }

    async fn cancel_fanout_group_with_origin(
        &self,
        group_id: &str,
        reason: &str,
        origin: CancellationOrigin,
    ) -> Option<FanoutGroupCancellation> {
        let _activity = self.begin_lifecycle_activity();
        // Linearize unassigned settlement and accepted-agent discovery under
        // the same group lock used by spawn acceptance. If cancellation wins,
        // a late spawn observes a terminal fixed slot and cannot enter the
        // provider; if acceptance wins, its exact agent id is cancelled below.
        let (group, mut active_agent_ids, already_terminal_count, non_stoppable_count) = {
            let mut groups = self.fanout_groups.write().await;
            let group = groups.get_mut(group_id)?;
            let already_terminal_count = group
                .slots
                .iter()
                .filter(|slot| slot.status.is_terminal())
                .count();
            let non_stoppable_count = group
                .slots
                .iter()
                .filter(|slot| !slot.status.is_terminal() && slot.agent_id.is_none())
                .count();
            let active_agent_ids = group
                .slots
                .iter()
                .filter(|slot| !slot.status.is_terminal())
                .filter_map(|slot| slot.agent_id.clone())
                .collect::<Vec<_>>();

            if matches!(
                origin,
                CancellationOrigin::User | CancellationOrigin::Runtime
            ) {
                let unassigned_status = if origin == CancellationOrigin::User {
                    AgentFanoutSlotStatus::CancelledByUser
                } else {
                    AgentFanoutSlotStatus::CancelledByRuntime
                };
                for slot_index in 0..group.slots.len() {
                    if group.slots[slot_index].agent_id.is_none()
                        && !group.slots[slot_index].status.is_terminal()
                    {
                        let _ =
                            group.record_unassigned_terminal(slot_index, unassigned_status, reason);
                    }
                }
                group.touch();
                self.publish_fanout_group(group);
                self.fanout_terminal_result_cache
                    .write()
                    .await
                    .remove(group_id);
            }
            (
                group.clone(),
                active_agent_ids,
                already_terminal_count,
                non_stoppable_count,
            )
        };
        // A spawn future may be waiting on mailbox/worktree/trace setup after
        // reserving local capacity but before attaching its identity to the
        // group slot. The group deadline owns those reservations too. Recover
        // them from the typed fanout identity on active state so dropping the
        // join future cannot leak an executing child or capacity.
        {
            let active = self.active_agents.read().await;
            for state in active.values() {
                if state
                    .fanout_slot
                    .as_ref()
                    .is_some_and(|slot| slot.group_id == group_id)
                    && !active_agent_ids.contains(&state.agent_id)
                {
                    active_agent_ids.push(state.agent_id.clone());
                }
            }
        }
        let cancellation_results = self
            .cancel_agents_with_origin_locally(active_agent_ids, reason, origin)
            .await;
        let updated = self
            .fanout_groups
            .read()
            .await
            .get(group_id)
            .cloned()
            .unwrap_or(group);
        let mut cancellation_pending_agent_ids = Vec::new();
        let mut stopped_agent_ids = Vec::new();
        let mut not_stopped_agent_ids = Vec::new();
        for (agent_id, outcome) in cancellation_results {
            let terminal = updated.slots.iter().any(|slot| {
                slot.agent_id.as_deref() == Some(agent_id.as_str()) && slot.status.is_terminal()
            });
            if terminal {
                stopped_agent_ids.push(agent_id);
            } else if outcome.is_pending() {
                cancellation_pending_agent_ids.push(agent_id);
            } else {
                not_stopped_agent_ids.push(agent_id);
            }
        }
        Some(FanoutGroupCancellation {
            group: updated,
            cancellation_pending_agent_ids,
            stopped_agent_ids,
            not_stopped_agent_ids,
            already_terminal_count,
            non_stoppable_count,
        })
    }

    /// Cancel every live dynamic-agent descendant of `parent_run_id`.
    ///
    /// Dynamic fanout tasks are owned by the session spawner rather than the
    /// parent loop's `JoinHandle`, so dropping/cancelling the parent future is
    /// not sufficient. Snapshot the run tree first, then cancel deepest-first
    /// without holding an agent-map lock across persistence or mailbox I/O.
    pub async fn cancel_descendants_of_parent_run_for_user(&self, parent_run_id: &str) -> usize {
        let _activity = self.begin_lifecycle_activity();
        let origin = CancellationOrigin::User;
        let reason = DescendantCancellationReason::ancestor_cancelled(origin).as_str();
        // Serialize the snapshot boundary with spawn reservation. Any spawn
        // already holding the read fence finishes insertion and is included;
        // any later spawn observes the cancellation marker and is rejected.
        let mut cancellation_fence = self.cancelling_parent_runs.write().await;
        cancellation_fence.insert(parent_run_id.to_string());
        let descendants = self
            .local_descendant_seizure_order(&mut cancellation_fence, parent_run_id)
            .await;
        drop(cancellation_fence);

        self.cancel_agents_with_origin_locally(descendants, reason, origin)
            .await
            .into_iter()
            .filter(|(_, outcome)| outcome.owns_local_stop())
            .count()
    }

    /// Snapshot one local run subtree while holding the spawn fence and
    /// return deepest descendants first. The fence is extended to every run
    /// in the snapshot before it is released, so no descendant can create a
    /// new unvisited generation between discovery and local seizure.
    async fn local_descendant_seizure_order(
        &self,
        cancellation_fence: &mut HashSet<String>,
        parent_run_id: &str,
    ) -> Vec<String> {
        // Lock order is deliberately in-flight -> active/completed, matching
        // both seizure (in-flight -> active) and archived cancellation
        // (in-flight -> completed). Holding the stable in-flight view across
        // both collection reads closes every container-handoff window without
        // a process-wide lock or retrying/global rescans.
        let pending_cancellations = self.in_flight_cancellations.read().await;
        let mut children_by_parent: HashMap<String, Vec<(String, String, bool)>> = HashMap::new();
        let mut seen_agents = HashSet::new();
        {
            let active = self.active_agents.read().await;
            for state in active.values() {
                seen_agents.insert(state.agent_id.clone());
                children_by_parent
                    .entry(state.parent_run_id.clone())
                    .or_default()
                    .push((
                        state.agent_id.clone(),
                        state.run_id.clone(),
                        !state.status.is_terminal(),
                    ));
            }
        }
        {
            // A durable child may have yielded a resumable `Waiting` state and
            // therefore no longer own an executor task. It is still part of
            // the live descendant graph and must inherit ancestor
            // cancellation. Walk newest archived state first so an older
            // non-terminal projection can never supersede a later terminal
            // observation of the same agent.
            let archived = self.completed_agents.read().await;
            for state in archived.iter().rev() {
                if !seen_agents.insert(state.agent_id.clone()) {
                    continue;
                }
                children_by_parent
                    .entry(state.parent_run_id.clone())
                    .or_default()
                    .push((
                        state.agent_id.clone(),
                        state.run_id.clone(),
                        !state.status.is_terminal(),
                    ));
            }
        }

        #[cfg(test)]
        let snapshot_hook = self
            .descendant_snapshot_before_pending_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        #[cfg(test)]
        if let Some((entered, release)) = snapshot_hook {
            entered.notify_one();
            release.notified().await;
        }

        // Cancellation owns the state before it disappears from `active` and
        // keeps that exact generation while its pending archive is published.
        // The read guard was acquired before either collection, so a terminal
        // finalizer cannot remove this connector between snapshots.
        for (agent_id, job) in pending_cancellations.iter() {
            if !seen_agents.insert(agent_id.clone()) {
                continue;
            }
            children_by_parent
                .entry(job.parent_run_id.clone())
                .or_default()
                .push((agent_id.clone(), job.run_id.clone(), true));
        }
        drop(pending_cancellations);

        let mut pending = VecDeque::from([(parent_run_id.to_string(), 0usize)]);
        let mut visited_runs = HashSet::new();
        let mut descendants = Vec::new();
        while let Some((run_id, depth)) = pending.pop_front() {
            if !visited_runs.insert(run_id.clone()) {
                continue;
            }
            let Some(children) = children_by_parent.get(&run_id) else {
                continue;
            };
            for (agent_id, child_run_id, seizable) in children {
                if *seizable {
                    descendants.push((depth + 1, agent_id.clone()));
                }
                cancellation_fence.insert(child_run_id.clone());
                pending.push_back((child_run_id.clone(), depth + 1));
            }
        }
        descendants.sort_by_key(|(depth, _)| std::cmp::Reverse(*depth));
        descendants
            .into_iter()
            .map(|(_, agent_id)| agent_id)
            .collect()
    }

    async fn cancel_user_agent_subtree_owned(
        &self,
        agent_id: &str,
        reason: &str,
    ) -> CancellationTransferOutcome {
        let mut cancellation_fence = self.cancelling_parent_runs.write().await;
        let target_run_id = self
            .active_agents
            .read()
            .await
            .get(agent_id)
            .filter(|state| !state.status.is_terminal())
            .map(|state| state.run_id.clone());
        let target_run_id = match target_run_id {
            Some(run_id) => Some(run_id),
            None => {
                let archived = self
                    .completed_agents
                    .read()
                    .await
                    .iter()
                    .rev()
                    .find(|state| state.agent_id == agent_id && !state.status.is_terminal())
                    .map(|state| state.run_id.clone());
                match archived {
                    Some(run_id) => Some(run_id),
                    None => self
                        .in_flight_cancellations
                        .read()
                        .await
                        .get(agent_id)
                        .map(|job| job.run_id.clone()),
                }
            }
        };
        let Some(target_run_id) = target_run_id else {
            drop(cancellation_fence);
            return self
                .cancel_agent_with_origin(agent_id, reason, CancellationOrigin::User)
                .await;
        };
        cancellation_fence.insert(target_run_id.clone());
        let mut seizure_order = self
            .local_descendant_seizure_order(&mut cancellation_fence, &target_run_id)
            .await;
        seizure_order.push(agent_id.to_string());
        drop(cancellation_fence);

        self.cancel_agents_with_origin_locally(seizure_order, reason, CancellationOrigin::User)
            .await
            .into_iter()
            .find_map(|(candidate, outcome)| (candidate == agent_id).then_some(outcome))
            .unwrap_or(CancellationTransferOutcome::NotFound)
    }

    async fn cancel_agent_with_origin(
        &self,
        agent_id: &str,
        reason: &str,
        origin: CancellationOrigin,
    ) -> CancellationTransferOutcome {
        let _activity = self.begin_lifecycle_activity();
        self.cancel_agents_with_origin_locally(vec![agent_id.to_string()], reason, origin)
            .await
            .into_iter()
            .next()
            .map(|(_, outcome)| outcome)
            .unwrap_or(CancellationTransferOutcome::NotFound)
    }

    /// Transfer local execution and durable reconciliation ownership for a
    /// batch without performing durable I/O in the request task. The detached
    /// owner is deliberate: cancelling an HTTP/CLI future while a large
    /// fanout is being seized must not leave the unvisited suffix executing.
    async fn cancel_agents_with_origin_locally(
        &self,
        agent_ids: Vec<String>,
        reason: &str,
        origin: CancellationOrigin,
    ) -> Vec<(String, CancellationTransferOutcome)> {
        let spawner = self.clone_for_task();
        let reason = reason.to_string();
        let fallback_ids = agent_ids.clone();
        let owner = tokio::spawn(async move {
            let _activity = spawner.begin_lifecycle_activity();
            let mut results = Vec::with_capacity(agent_ids.len());
            let mut admissions = Vec::with_capacity(agent_ids.len());
            for agent_id in agent_ids {
                let transfer = spawner
                    .seize_agent_and_schedule_cancellation(&agent_id, &reason, origin)
                    .await;
                if let Some(admission) = transfer.admission {
                    admissions.push(admission);
                }
                results.push((agent_id, transfer.outcome));
            }
            // Publish the whole local ownership transfer before any durable
            // worker can dequeue a prefix. This makes a subtree cancellation
            // one fence: descendants and target are all stopped even if the
            // caller disappears or the first durable CAS stalls.
            spawner.admit_durable_cancellation_batch(admissions).await;
            spawner.ensure_cancellation_retry_supervisor();
            results
        });
        owner.await.unwrap_or_else(|error| {
            astra_core::agent_warn!(
                "spawner",
                "local cancellation owner failed before completing its batch: {error}"
            );
            fallback_ids
                .into_iter()
                .map(|agent_id| (agent_id, CancellationTransferOutcome::NotFound))
                .collect()
        })
    }

    async fn admit_durable_cancellation_batch(
        &self,
        admissions: Vec<DurableCancellationAdmission>,
    ) {
        let admitted = {
            let mut pending = self.in_flight_cancellations.write().await;
            admissions
                .into_iter()
                .filter_map(|admission| {
                    let job = pending.get_mut(&admission.agent_id)?;
                    if job.run_id != admission.run_id
                        || job.owner_version != admission.owner_version
                        || job.durable_ready
                        || job.finalizing
                    {
                        return None;
                    }
                    job.durable_ready = true;
                    Some((admission.agent_id, admission.run_id))
                })
                .collect::<Vec<_>>()
        };
        for (agent_id, run_id) in admitted {
            self.schedule_durable_cancellation(agent_id, run_id, tokio::time::Instant::now(), true);
        }
    }

    fn upgrade_in_flight_cancellation(
        current: &mut InFlightCancellation,
        origin: CancellationOrigin,
        reason: &str,
    ) -> bool {
        if current.finalizing
            || cancellation_origin_precedence(origin)
                <= cancellation_origin_precedence(current.origin)
        {
            return false;
        }
        let stale_owner = std::mem::replace(
            &mut current.owner_changed,
            tokio_util::sync::CancellationToken::new(),
        );
        current.origin = origin;
        current.reason = reason.to_string();
        current.owner_version = current.owner_version.saturating_add(1);
        current.retry_count = 0;
        current.durable_ready = false;
        stale_owner.cancel();
        true
    }

    async fn seize_agent_and_schedule_cancellation(
        &self,
        agent_id: &str,
        reason: &str,
        origin: CancellationOrigin,
    ) -> LocalCancellationTransfer {
        // Single write-lock scope removes both the abort handle and agent
        // state as one local ownership transfer. This prevents a TOCTOU race where the
        // monitor finalises the agent between handle removal and state
        // finalization — which would leave the cancel_agent caller thinking
        // it "won" while the monitor already cleaned up (and possibly recorded
        // a different terminal status for fanout slots).
        let seized_active = {
            // The exact durable owner is installed before publishing the
            // active-map removal. Concurrent cancellation of the same stable
            // agent id can therefore observe one owner but can never replace
            // its generation/binding job.
            let mut pending = self.in_flight_cancellations.write().await;
            if let Some(current) = pending.get_mut(agent_id) {
                let upgraded = Self::upgrade_in_flight_cancellation(current, origin, reason);
                return LocalCancellationTransfer {
                    outcome: CancellationTransferOutcome::AlreadyPending,
                    admission: upgraded.then(|| DurableCancellationAdmission {
                        agent_id: agent_id.to_string(),
                        run_id: current.run_id.clone(),
                        owner_version: current.owner_version,
                    }),
                };
            }
            let mut handles = self.background_abort_handles.write().await;
            let handle = handles.remove(agent_id);
            // Every typed cancellation origin owns active reservations whose
            // executor abort handle has not been installed yet. In particular,
            // ancestor user cancellation must seize an Initializing child at
            // this boundary so setup cannot escape after the root is deleted.
            let state = {
                let mut active_agents = self.active_agents.write().await;
                // A normal finalizer publishes its terminal state in the
                // active map before updating the fanout group and archive.
                // Never seize or overwrite that completed result. The caller
                // will use the visible state below to close any lagging group
                // projection deterministically.
                if active_agents
                    .get(agent_id)
                    .is_some_and(|state| state.status.is_terminal())
                {
                    None
                } else {
                    active_agents.remove(agent_id)
                }
            };
            match state {
                Some(state) => {
                    pending.insert(
                        agent_id.to_string(),
                        InFlightCancellation {
                            origin,
                            reason: reason.to_string(),
                            run_id: state.run_id.clone(),
                            parent_run_id: state.parent_run_id.clone(),
                            cancellation_binding_id: state.cancellation_binding_id.clone(),
                            user_id: state
                                .trace_context
                                .as_ref()
                                .map(|trace| trace.user_id.clone()),
                            owner_version: 1,
                            owner_changed: tokio_util::sync::CancellationToken::new(),
                            durable_ready: false,
                            finalizing: false,
                            retry_count: 0,
                        },
                    );
                    Some((handle.filter(|handle| !handle.is_finished()), state, 1))
                }
                None => {
                    if let Some(handle) = handle.filter(|handle| !handle.is_finished()) {
                        handles.insert(agent_id.to_string(), handle);
                    }
                    None
                }
            }
        };

        let Some((abort_handle, mut state, owner_version)) = seized_active else {
            // The executor may have completed and published a terminal active
            // projection while its finalizer is still crossing the local
            // group/archive handoff. Help that owner close the group before a
            // deadline render observes an active/unknown slot. This is local,
            // idempotent, and preserves the completed result over cancellation.
            if let Some(terminal_state) = self
                .active_agents
                .read()
                .await
                .get(agent_id)
                .filter(|state| state.status.is_terminal())
                .cloned()
            {
                self.record_fanout_terminal_state(&terminal_state).await;
                return LocalCancellationTransfer::observed(
                    CancellationTransferOutcome::AlreadyTerminal,
                );
            }
            return self
                .cancel_archived_nonterminal_agent(agent_id, reason, origin)
                .await;
        };

        // Local execution stops before any projection or durable await. The
        // in-flight map already owns the exact binding if the request future
        // disappears at any later point.
        if let Some(abort_handle) = abort_handle {
            abort_handle.abort();
        }
        #[cfg(test)]
        let cancellation_hook = {
            self.cancellation_before_in_flight_hook
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        };
        #[cfg(test)]
        if let Some((entered, release)) = cancellation_hook {
            entered.notify_one();
            release.notified().await;
        }
        self.archive_pending_seized_agent_projection(&mut state, agent_id)
            .await;
        LocalCancellationTransfer {
            outcome: CancellationTransferOutcome::SeizedPending,
            admission: Some(DurableCancellationAdmission {
                agent_id: agent_id.to_string(),
                run_id: state.run_id,
                owner_version,
            }),
        }
    }

    /// Cancel a resumable/non-terminal child whose executor invocation has
    /// already returned (for example a durable `Waiting` result). Such a child
    /// has no live `AbortHandle`, but it still owns a non-terminal run and
    /// fanout slot. Cancellation therefore targets durable run control first
    /// and only then publishes the terminal local projection.
    async fn cancel_archived_nonterminal_agent(
        &self,
        agent_id: &str,
        reason: &str,
        origin: CancellationOrigin,
    ) -> LocalCancellationTransfer {
        let archived = {
            let mut pending = self.in_flight_cancellations.write().await;
            if let Some(current) = pending.get_mut(agent_id) {
                let upgraded = Self::upgrade_in_flight_cancellation(current, origin, reason);
                return LocalCancellationTransfer {
                    outcome: CancellationTransferOutcome::AlreadyPending,
                    admission: upgraded.then(|| DurableCancellationAdmission {
                        agent_id: agent_id.to_string(),
                        run_id: current.run_id.clone(),
                        owner_version: current.owner_version,
                    }),
                };
            }
            let mut completed = self.completed_agents.write().await;
            let Some(position) = completed
                .iter()
                .rposition(|state| state.agent_id == agent_id && !state.status.is_terminal())
            else {
                return if completed
                    .iter()
                    .any(|state| state.agent_id == agent_id && state.status.is_terminal())
                {
                    LocalCancellationTransfer::observed(
                        CancellationTransferOutcome::AlreadyTerminal,
                    )
                } else {
                    LocalCancellationTransfer::observed(CancellationTransferOutcome::NotFound)
                };
            };
            let archived = completed.remove(position);
            if let Some(state) = archived.as_ref() {
                pending.insert(
                    agent_id.to_string(),
                    InFlightCancellation {
                        origin,
                        reason: reason.to_string(),
                        run_id: state.run_id.clone(),
                        parent_run_id: state.parent_run_id.clone(),
                        cancellation_binding_id: state.cancellation_binding_id.clone(),
                        user_id: state
                            .trace_context
                            .as_ref()
                            .map(|trace| trace.user_id.clone()),
                        owner_version: 1,
                        owner_changed: tokio_util::sync::CancellationToken::new(),
                        durable_ready: false,
                        finalizing: false,
                        retry_count: 0,
                    },
                );
            }
            archived
        };
        let Some(mut state) = archived else {
            return LocalCancellationTransfer::observed(CancellationTransferOutcome::NotFound);
        };
        self.archive_pending_seized_agent_projection(&mut state, agent_id)
            .await;
        LocalCancellationTransfer {
            outcome: CancellationTransferOutcome::SeizedPending,
            admission: Some(DurableCancellationAdmission {
                agent_id: agent_id.to_string(),
                run_id: state.run_id,
                owner_version: 1,
            }),
        }
    }

    async fn publish_seized_agent_projection(&self, state: &SpawnedAgentState, agent_id: &str) {
        self.record_fanout_terminal_state(state).await;
        self.publish_background_agent(state);
        if let Some(event_type) =
            agent_status_to_progress_event(&state.status, &state.metrics, state.started_at)
        {
            let timestamp_epoch_ms = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
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
        self.archive_state(state.clone()).await;
    }

    /// Publish the durable cancellation request as pending without releasing
    /// resources needed to deliver the eventual authoritative terminal. A
    /// recovery intent or control-plane error is not a cancellation winner.
    async fn archive_pending_seized_agent_projection(
        &self,
        state: &mut SpawnedAgentState,
        agent_id: &str,
    ) {
        state.status = AgentStatus::Waiting {
            reason: "durable cancellation reconciliation pending".to_string(),
        };
        state.work_revision = state.work_revision.saturating_add(1);
        state.ended_at = None;
        self.publish_seized_agent_projection(state, agent_id).await;
    }

    /// Release this process's obsolete execution resources after the durable
    /// owner proves a different active generation. This remains non-terminal:
    /// no parent result or completion notification is emitted.
    async fn release_seized_agent_projection(
        &self,
        state: &mut SpawnedAgentState,
        agent_id: &str,
        status: AgentStatus,
    ) -> bool {
        debug_assert!(!status.is_terminal());
        self.foreground_promotion_requests
            .write()
            .await
            .remove(agent_id);
        state.status = status;
        state.work_revision = state.work_revision.saturating_add(1);
        state.ended_at = None;
        let messaging_address = state.messaging_address.take();
        let worktree_path = state.worktree_path.take();
        let projected = state.clone();
        self.publish_seized_agent_projection(&projected, agent_id)
            .await;

        let spawner = self.clone_for_task();
        let agent_id = agent_id.to_string();
        tokio::spawn(async move {
            spawner.cleanup_worktree(worktree_path, &agent_id).await;
            if let Some(address) = messaging_address {
                match tokio::time::timeout(
                    AGENT_MAILBOX_UNREGISTER_TIMEOUT,
                    spawner.mailbox_router.unregister(&address),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::warn!(
                        target: "astra_runtime::messaging",
                        %agent_id,
                        %error,
                        "seized child cleanup could not unregister mailbox"
                    ),
                    Err(_) => tracing::warn!(
                        target: "astra_runtime::messaging",
                        %agent_id,
                        "seized child cleanup timed out unregistering mailbox"
                    ),
                }
            }
        });
        true
    }

    /// Publish one authoritative terminal for an execution seized by the
    /// cancellation path. Every durable terminal winner uses this same
    /// finalizer, whether it is Cancelled or an opposite Completed/Failed
    /// result, so parent delivery, journal, trace, fanout and waiters cannot
    /// disagree.
    async fn finalize_authoritative_seized_agent(
        &self,
        state: &mut SpawnedAgentState,
        agent_id: &str,
        status: AgentStatus,
    ) -> bool {
        debug_assert!(status.is_terminal());
        self.foreground_promotion_requests
            .write()
            .await
            .remove(agent_id);

        let (terminal_status, finish_reason, output, error) = match &status {
            AgentStatus::Completed {
                result,
                finish_reason,
            } => (
                SPAWN_STATUS_COMPLETED,
                finish_reason.clone(),
                Some(result.clone()),
                None,
            ),
            AgentStatus::Interrupted {
                partial_result,
                finish_reason,
            } => (
                SPAWN_STATUS_INTERRUPTED,
                Some(finish_reason.clone()),
                Some(partial_result.clone()),
                None,
            ),
            AgentStatus::Failed {
                error,
                finish_reason,
            } => (
                SPAWN_STATUS_FAILED,
                finish_reason.clone(),
                None,
                Some(error.clone()),
            ),
            AgentStatus::Cancelled { reason, .. } => (
                SPAWN_STATUS_CANCELLED,
                Some(reason.clone()),
                Some(reason.clone()),
                None,
            ),
            AgentStatus::Initializing
            | AgentStatus::Running { .. }
            | AgentStatus::Idle
            | AgentStatus::Waiting { .. } => {
                debug_assert!(false, "authoritative finalizer requires terminal status");
                return false;
            }
        };
        state.status = status;
        state.work_revision = state.work_revision.saturating_add(1);
        state.ended_at = Some(SystemTime::now());
        let messaging_address = state.messaging_address.take();

        let worktree_path = state.worktree_path.take();
        let settled_state = state.clone();
        self.publish_seized_agent_projection(&settled_state, agent_id)
            .await;
        self.notify_completion(agent_id).await;

        let spawner = self.clone_for_task();
        let agent_id = agent_id.to_string();
        let terminal_status = terminal_status.to_string();
        tokio::spawn(async move {
            let cleanup = async {
                // Worktree ownership is process-local and precedes all
                // transport-backed best-effort observability.
                spawner.cleanup_worktree(worktree_path, &agent_id).await;
                match tokio::time::timeout(
                    AGENT_TERMINAL_JOURNAL_TIMEOUT,
                    spawner.persist_agent_terminated_state(
                        &settled_state,
                        &terminal_status,
                        finish_reason.as_deref(),
                    ),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::warn!(
                        target: "fanout",
                        %agent_id,
                        %error,
                        "could not persist authoritative child terminal evidence"
                    ),
                    Err(_) => tracing::warn!(
                        target: "fanout",
                        %agent_id,
                        timeout_ms = AGENT_TERMINAL_JOURNAL_TIMEOUT.as_millis() as u64,
                        "authoritative child journal append exceeded its local terminal bound"
                    ),
                }
                if let Some(addr) = messaging_address {
                    let _ = tokio::time::timeout(
                        AGENT_TERMINAL_DELIVERY_TIMEOUT,
                        spawner.deliver_terminal_result_to_parent(&settled_state, &addr),
                    )
                    .await;
                    match tokio::time::timeout(
                        AGENT_MAILBOX_UNREGISTER_TIMEOUT,
                        spawner.mailbox_router.unregister(&addr),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => tracing::warn!(
                            target: "astra_runtime::messaging",
                            %agent_id,
                            %error,
                            "authoritative child cleanup could not unregister mailbox"
                        ),
                        Err(_) => tracing::warn!(
                            target: "astra_runtime::messaging",
                            %agent_id,
                            "authoritative child cleanup timed out unregistering mailbox"
                        ),
                    }
                }
                spawner
                    .emit_agent_terminal_trace(
                        &settled_state,
                        &terminal_status,
                        finish_reason.as_deref(),
                        output.as_deref(),
                        error.as_deref(),
                    )
                    .await;
            };
            if tokio::time::timeout(AGENT_DEADLINE_CLEANUP_TIMEOUT, cleanup)
                .await
                .is_err()
            {
                tracing::warn!(
                    target: "fanout",
                    %agent_id,
                    deadline_ms = AGENT_DEADLINE_CLEANUP_TIMEOUT.as_millis() as u64,
                    "authoritative child cleanup exceeded its best-effort bound"
                );
            }
        });
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
        let (mut state, messaging_address) = {
            // Compete with deadline cancellation under the same handles ->
            // active lock order. Neither owner may remove half the lifecycle
            // pair and let the other overwrite a completed partial/result.
            let mut handles = self.background_abort_handles.write().await;
            handles.remove(agent_id);
            let mut active_agents = self.active_agents.write().await;
            let Some(state) = active_agents.get_mut(agent_id) else {
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
            (state.clone(), messaging_address)
        };

        self.record_fanout_terminal_state(&state).await;
        self.publish_background_agent(&state);
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
        let worktree_path = state.worktree_path.take();
        let settled_state = state.clone();
        // Publish queryable local terminal state before any trace, journal,
        // mailbox, or filesystem await. Fanout aggregation must never lose a
        // completed/partial result merely because best-effort persistence is
        // slow at the group deadline.
        self.archive_state(settled_state.clone()).await;
        // Only now retire the active handoff. Until both the group and archive
        // contain the terminal state, deadline cancellation can still query
        // the active map and help publish the same terminal projection.
        self.active_agents.write().await.remove(agent_id);
        self.notify_completion(agent_id).await;
        let spawner = self.clone_for_task();
        let agent_id = agent_id.to_string();
        let journal_status = journal_status.to_string();
        let finish_reason = finish_reason.map(ToString::to_string);
        let output = output.map(ToString::to_string);
        let error = error.map(ToString::to_string);
        tokio::spawn(async move {
            let cleanup = async {
                // Local lifecycle ownership is correctness-critical; trace
                // and journal persistence are best-effort observability. Do
                // not let a blocking persistence backend retain these
                // resources past terminal publication.
                // Release the local filesystem resource first. Mailbox send
                // and unregister may each cross a database-backed transport.
                spawner.cleanup_worktree(worktree_path, &agent_id).await;
                if let Some(addr) = messaging_address {
                    let _ = tokio::time::timeout(
                        AGENT_TERMINAL_DELIVERY_TIMEOUT,
                        spawner.deliver_terminal_result_to_parent(&settled_state, &addr),
                    )
                    .await;
                    match tokio::time::timeout(
                        AGENT_MAILBOX_UNREGISTER_TIMEOUT,
                        spawner.mailbox_router.unregister(&addr),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => tracing::warn!(
                            target: "astra_runtime::messaging",
                            %agent_id,
                            %err,
                            "terminal child cleanup could not unregister mailbox"
                        ),
                        Err(_) => tracing::warn!(
                            target: "astra_runtime::messaging",
                            %agent_id,
                            "terminal child cleanup timed out unregistering mailbox"
                        ),
                    }
                }
                spawner
                    .emit_agent_terminal_trace(
                        &settled_state,
                        &journal_status,
                        finish_reason.as_deref(),
                        output.as_deref(),
                        error.as_deref(),
                    )
                    .await;
                let _ = spawner
                    .persist_agent_terminated_state(
                        &settled_state,
                        &journal_status,
                        finish_reason.as_deref(),
                    )
                    .await;
            };
            if tokio::time::timeout(AGENT_DEADLINE_CLEANUP_TIMEOUT, cleanup)
                .await
                .is_err()
            {
                tracing::warn!(
                    target: "fanout",
                    %agent_id,
                    deadline_ms = AGENT_DEADLINE_CLEANUP_TIMEOUT.as_millis() as u64,
                    "terminal child cleanup exceeded its best-effort bound"
                );
            }
        });
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

    async fn append_owned_journal_event(
        &self,
        user_id: Option<String>,
        session_id: String,
        event: astra_services::session_journal::JournalEvent,
    ) -> Result<(), String> {
        let journal_dir_override = self
            .journal_dir_override
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        tokio::task::spawn_blocking(move || {
            let _journal_dir_guard = journal_dir_override
                .as_ref()
                .map(astra_services::session_journal::JournalDirGuard::new);
            let writer = match user_id.as_deref() {
                Some(user_id) => {
                    astra_services::session_journal::JournalWriter::for_user(user_id, &session_id)
                }
                None => astra_services::session_journal::JournalWriter::new(&session_id),
            }
            .map_err(|error| format!("journal writer init failed: {error}"))?;
            writer
                .append(&event)
                .map_err(|error| format!("journal append failed: {error}"))
        })
        .await
        .map_err(|error| format!("journal blocking task failed: {error}"))?
    }

    /// Persist final agent state to session journal (best-effort for ordinary
    /// completion; cancellation ownership observes the returned error).
    async fn persist_agent_terminated_state(
        &self,
        state: &SpawnedAgentState,
        status: &str,
        finish_reason: Option<&str>,
    ) -> Result<(), String> {
        let Some(sid) = self.current_session_id() else {
            return Err("agent terminal has no bound session journal".to_string());
        };
        let state = state.clone();
        let status = status.to_string();
        let finish_reason = finish_reason.map(ToString::to_string);
        let user_id = state
            .trace_context
            .as_ref()
            .map(|trace| trace.user_id.clone());
        let event = {
            let duration_ms = state
                .started_at
                .elapsed()
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(0);
            let metrics_completeness =
                (status == "cancelled").then_some("unknown_after_forced_cancellation");
            astra_services::session_journal::JournalEvent::agent_terminated_with_metric_completeness(
                Some(sid.as_str()),
                &state.agent_id,
                &state.run_id,
                &state.agent_type,
                &status,
                finish_reason.as_deref(),
                (state.metrics.turns_completed > 0).then_some(state.metrics.turns_completed),
                state.metrics.tool_calls,
                state.metrics.prompt_tokens,
                state.metrics.completion_tokens,
                duration_ms,
                metrics_completeness,
                state.execution_metadata.as_ref(),
            )
        };
        self.append_owned_journal_event(user_id, sid, event).await
    }

    async fn archive_state(&self, state: SpawnedAgentState) {
        let mut completed = self.completed_agents.write().await;
        const MAX_COMPLETED_AGENTS: usize = 256;
        if completed.len() >= MAX_COMPLETED_AGENTS {
            // Non-terminal entries may own the mailbox/worktree capability
            // needed by durable cancellation reconciliation. Evict history,
            // never live ownership. If every entry is non-terminal, allow the
            // queue to exceed the history target until one settles; active and
            // in-flight admission bounds that exceptional growth.
            if let Some(position) = completed
                .iter()
                .position(|archived| archived.status.is_terminal())
            {
                completed.remove(position);
            }
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
            activity_epoch: Arc::clone(&self.activity_epoch),
            lifecycle_activity_count: Arc::clone(&self.lifecycle_activity_count),
            cancelling_parent_runs: Arc::clone(&self.cancelling_parent_runs),
            progress_broadcaster: Arc::clone(&self.progress_broadcaster),
            context_cache: Arc::clone(&self.context_cache),
            executor: self.executor.clone(),
            session_id: self.session_id.clone(),
            journal_dir_override: Arc::clone(&self.journal_dir_override),
            agent_registry: self.agent_registry.clone(),
            completed_agents: Arc::clone(&self.completed_agents),
            _background_task_owner: None,
            background_tasks: self.background_tasks.clone(),
            background_task_admission: Arc::clone(&self.background_task_admission),
            background_task_shutdown: self.background_task_shutdown.clone(),
            #[cfg(test)]
            spawn_preparation_gate: Arc::clone(&self.spawn_preparation_gate),
            #[cfg(test)]
            spawn_before_reservation_barriers: Arc::clone(&self.spawn_before_reservation_barriers),
            #[cfg(test)]
            cancellation_before_in_flight_hook: Arc::clone(
                &self.cancellation_before_in_flight_hook,
            ),
            #[cfg(test)]
            descendant_snapshot_before_pending_hook: Arc::clone(
                &self.descendant_snapshot_before_pending_hook,
            ),
            #[cfg(test)]
            shutdown_active_snapshot_count: Arc::clone(&self.shutdown_active_snapshot_count),
            background_abort_handles: Arc::clone(&self.background_abort_handles),
            in_flight_cancellations: Arc::clone(&self.in_flight_cancellations),
            cancellation_retry_supervisor_running: Arc::clone(
                &self.cancellation_retry_supervisor_running,
            ),
            cancellation_retry_queue: Arc::clone(&self.cancellation_retry_queue),
            cancellation_retry_sequence: Arc::clone(&self.cancellation_retry_sequence),
            cancellation_retry_wakeup: Arc::clone(&self.cancellation_retry_wakeup),
            #[cfg(test)]
            cancellation_capacity_override: Arc::clone(&self.cancellation_capacity_override),
            #[cfg(test)]
            cancellation_capacity_waiting_hook: Arc::clone(
                &self.cancellation_capacity_waiting_hook,
            ),
            #[cfg(test)]
            cancellation_retry_panic_after_dequeue: Arc::clone(
                &self.cancellation_retry_panic_after_dequeue,
            ),
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
        let _activity = self.begin_lifecycle_activity();
        let shutdown_deadline = tokio::time::Instant::now() + deadline;
        // Close admission synchronously and cancel long-running preparation.
        // Final task installation takes this same short lock, so it is either
        // visible in the JoinSet below or rejected and locally terminalized.
        let mut set = {
            let mut admission = self
                .background_task_admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *admission = false;
            self.background_task_shutdown.cancel();
            self.background_tasks.upgrade().map(|tasks| {
                tasks
                    .lock()
                    .map(|mut guard| std::mem::take(&mut *guard))
                    .unwrap_or_else(|poisoned| {
                        let mut guard = poisoned.into_inner();
                        std::mem::take(&mut *guard)
                    })
            })
        };

        // Preserve time for cancellation ownership transfer. Waiting for the
        // whole caller deadline here and then doing N sequential durable
        // cancels made shutdown scale as `deadline + N * cancel_timeout`.
        // Cancellation below is concurrent, but it still needs a bounded slice
        // in which to abort local executors and synchronously hand off their
        // terminal owners.
        let cancellation_reserve = deadline.min(AGENT_DURABLE_CANCEL_TIMEOUT);
        let passive_drain_budget = deadline.saturating_sub(cancellation_reserve);
        let joinset_drained = match tokio::time::timeout(passive_drain_budget, async {
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
            Ok(()) => true,
            Err(_) => {
                astra_core::agent_warn!(
                    "spawner",
                    "background agent passive drain timed out after {passive_drain_budget:?}; cancelling remaining tasks"
                );
                false
            }
        };
        if !joinset_drained && let Some(set) = set.as_mut() {
            // Do not wait for one cancellation RPC before stopping local
            // execution. JoinSet abort is process-local and makes every
            // provider/invocation guard drop promptly; durable child-run
            // cancellation remains owned by the supervised tasks below.
            set.abort_all();
        }

        // Preparation cancelled before JoinSet installation is represented in
        // `active_agents` but not in `set`. Converge every such process-local
        // reservation even when the JoinSet itself drained successfully.
        let unfinished = self
            .active_agents
            .read()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        #[cfg(test)]
        self.shutdown_active_snapshot_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Each task only performs the bounded local ownership transfer. The
        // scheduler observes the already-closed shutdown token and leaves the
        // durable marker explicitly pending for crash recovery; teardown must
        // neither start new backend I/O nor manufacture a terminal projection.
        let cancellation_tasks = unfinished
            .into_iter()
            .map(|agent_id| {
                let spawner = self.clone_for_task();
                let reason = reason.to_string();
                tokio::spawn(async move {
                    spawner
                        .cancel_agent_with_origin(&agent_id, &reason, CancellationOrigin::Runtime)
                        .await
                })
            })
            .collect::<Vec<_>>();
        let settle_owned_work = async {
            let drain_joinset = async {
                let Some(set) = set.as_mut() else {
                    return;
                };
                while set.join_next().await.is_some() {}
            };
            let await_cancellations = async {
                for task in cancellation_tasks {
                    let _ = task.await;
                }
            };
            tokio::join!(drain_joinset, await_cancellations);
        };
        let remaining = shutdown_deadline.saturating_duration_since(tokio::time::Instant::now());
        if tokio::time::timeout(remaining, settle_owned_work)
            .await
            .is_err()
        {
            astra_core::agent_warn!(
                "spawner",
                "background agent cancellation ownership did not settle within the shared {deadline:?} shutdown deadline"
            );
            if let Some(set) = set.as_mut() {
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

    /// True while a seized child still has an owned durable cancellation in
    /// progress. `active_agents` is deliberately cleared at seizure, so server
    /// shutdown must consult this owner set as well or it can report a clean
    /// drain before the child-run terminal CAS has settled.
    pub async fn has_in_flight_cancellation_owners(&self) -> bool {
        !self.in_flight_cancellations.read().await.is_empty()
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
        let _activity = self.begin_lifecycle_activity();
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

impl Drop for DynamicAgentSpawner {
    fn drop(&mut self) {
        if self._background_task_owner.is_some() {
            self.background_task_shutdown.cancel();
        }
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
    let reconstruction = reconstruct_messages(prefix, Vec::new());
    if astra_core::history_work::instrumentation_enabled() {
        astra_core::history_work::record_operation(
            astra_core::history_work::HistoryWorkSite::ForkPrefixReconstruction,
            u64::try_from(prefix.size_bytes()).unwrap_or(u64::MAX),
            reconstruction
                .as_ref()
                .map(|result| u64::try_from(result.prefix_len).unwrap_or(u64::MAX))
                .unwrap_or(0),
            0,
        );
    }
    match reconstruction {
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
    // A linked worktree has a `.git` file pointing at its parent repository.
    // Fallback isolation directories have no such marker and can be removed
    // directly by `cleanup_agent_worktree`, without spawning Git first.
    if !path.join(".git").is_file() {
        return Ok(false);
    }

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
    use astra_messaging::transport::{MessageStream, MessageTransport};
    use astra_messaging::types::{AgentMessage, MessagePayload, MessageTarget};
    use serde_json::json;
    use tokio::time::{Duration, sleep};

    fn mock_router() -> Arc<AgentMailboxRouter> {
        let transport = Arc::new(InProcessTransport::new());
        let dt = Arc::new(DelegationTracker::new());
        Arc::new(AgentMailboxRouter::new(transport, dt))
    }

    struct PendingTerminalTransport {
        send_started: Arc<std::sync::atomic::AtomicBool>,
        unregister_started: Arc<std::sync::atomic::AtomicBool>,
    }

    struct PendingRegisterTransport {
        register_started: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait]
    impl MessageTransport for PendingRegisterTransport {
        async fn register(
            &self,
            _addr: astra_messaging::AgentAddress,
            _delegation_id: Option<String>,
        ) -> Result<(), astra_messaging::MailboxError> {
            self.register_started
                .store(true, std::sync::atomic::Ordering::SeqCst);
            std::future::pending().await
        }

        async fn unregister(
            &self,
            _addr: &astra_messaging::AgentAddress,
        ) -> Result<(), astra_messaging::MailboxError> {
            Ok(())
        }

        async fn subscribe(
            &self,
            addr: &astra_messaging::AgentAddress,
        ) -> Result<Box<dyn MessageStream>, astra_messaging::MailboxError> {
            Err(astra_messaging::MailboxError::AgentNotFound(addr.clone()))
        }

        async fn resolve_agent(
            &self,
            _delegation_id: &str,
            _agent_id: &str,
        ) -> Result<astra_messaging::AgentAddress, astra_messaging::MailboxError> {
            Err(astra_messaging::MailboxError::Transport(
                "not used by spawn shutdown regression".to_string(),
            ))
        }

        async fn list_agents(
            &self,
            _delegation_id: &str,
        ) -> Result<Vec<astra_messaging::AgentAddress>, astra_messaging::MailboxError> {
            Ok(Vec::new())
        }

        async fn send(&self, _msg: Arc<AgentMessage>) -> Result<(), astra_messaging::MailboxError> {
            Ok(())
        }

        async fn broadcast(
            &self,
            _delegation_id: &str,
            _msg: Arc<AgentMessage>,
        ) -> Result<(), astra_messaging::MailboxError> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl MessageTransport for PendingTerminalTransport {
        async fn register(
            &self,
            _addr: astra_messaging::AgentAddress,
            _delegation_id: Option<String>,
        ) -> Result<(), astra_messaging::MailboxError> {
            Ok(())
        }

        async fn unregister(
            &self,
            _addr: &astra_messaging::AgentAddress,
        ) -> Result<(), astra_messaging::MailboxError> {
            self.unregister_started
                .store(true, std::sync::atomic::Ordering::SeqCst);
            std::future::pending().await
        }

        async fn subscribe(
            &self,
            addr: &astra_messaging::AgentAddress,
        ) -> Result<Box<dyn MessageStream>, astra_messaging::MailboxError> {
            Err(astra_messaging::MailboxError::AgentNotFound(addr.clone()))
        }

        async fn resolve_agent(
            &self,
            _delegation_id: &str,
            _agent_id: &str,
        ) -> Result<astra_messaging::AgentAddress, astra_messaging::MailboxError> {
            Err(astra_messaging::MailboxError::Transport(
                "not used by cleanup regression".to_string(),
            ))
        }

        async fn list_agents(
            &self,
            _delegation_id: &str,
        ) -> Result<Vec<astra_messaging::AgentAddress>, astra_messaging::MailboxError> {
            Ok(Vec::new())
        }

        async fn send(&self, _msg: Arc<AgentMessage>) -> Result<(), astra_messaging::MailboxError> {
            self.send_started
                .store(true, std::sync::atomic::Ordering::SeqCst);
            std::future::pending().await
        }

        async fn broadcast(
            &self,
            _delegation_id: &str,
            _msg: Arc<AgentMessage>,
        ) -> Result<(), astra_messaging::MailboxError> {
            std::future::pending().await
        }
    }

    #[test]
    fn per_spawn_tools_narrow_restricted_profiles_and_normalize_full_profiles() {
        let restricted = [" Bash ", "READ_FILE"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let requested = vec![
            " WRITE_FILE ".to_string(),
            "read_file".to_string(),
            "read_file".to_string(),
        ];
        assert_eq!(
            effective_spawn_allowed_tools(Some(&requested), &restricted),
            vec!["read_file".to_string()],
            "a read-only/restricted profile cannot be widened by spawn arguments"
        );

        let unrestricted = ["*".to_string()].into_iter().collect();
        assert_eq!(
            effective_spawn_allowed_tools(Some(&requested), &unrestricted),
            vec!["read_file".to_string(), "write_file".to_string()],
            "a full profile honors the normalized explicit child boundary"
        );

        let explore = astra_turn_core::orchestration_builtin_agents::get_builtin_agent_types()
            .into_iter()
            .find(|definition| definition.agent_type == "explore")
            .expect("explore profile");
        assert_eq!(
            effective_spawn_allowed_tools(Some(&["web_fetch".to_string()]), &explore.allowed_tools,),
            vec!["web_fetch".to_string()],
            "a read-only research profile must retain an explicitly narrowed read-only web capability"
        );
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
            model_offering_id: None,
            resolved_model_name: None,
            runtime_profile: None,
            start_request_fingerprint: None,
            work_binding: None,
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

    #[test]
    fn durable_completed_child_restores_with_completed_outcome_semantics() {
        let mut run = durable_run("completed-child", 1, astra_core::STATUS_COMPLETED);
        run.events.push(json!({
            "event_type": "text_done",
            "data": {"full_text": "Complete durable evidence."}
        }));

        assert!(matches!(
            durable_agent_status(&run),
            AgentStatus::Completed {
                result,
                finish_reason: Some(reason),
            } if result == "Complete durable evidence."
                && reason == astra_turn_core::orchestration_types::AGENT_FINISH_REASON_NORMAL
        ));
    }

    #[test]
    fn durable_status_without_payload_does_not_erase_owned_complete_result() {
        let run = durable_run("child-run", 1, astra_core::STATUS_COMPLETED);
        let current = AgentStatus::Completed {
            result: "Complete child evidence.".to_string(),
            finish_reason: Some("normal".to_string()),
        };

        assert_eq!(reconciled_durable_agent_status(&current, &run), current);
    }

    #[test]
    fn different_durable_terminal_state_overrides_owned_result() {
        let mut run = durable_run("child-run", 1, astra_core::STATUS_CANCELLED);
        run.error_message = Some("cancelled by durable owner".to_string());
        run.events.push(json!({
            "event_type": "run_finished",
            "data": {
                "cancelled": true,
                "reason": "runtime stopped child",
                "cancellation_origin": "runtime",
            }
        }));
        let current = AgentStatus::Completed {
            result: "stale local result".to_string(),
            finish_reason: Some("normal".to_string()),
        };

        assert!(matches!(
            reconciled_durable_agent_status(&current, &run),
            AgentStatus::Cancelled { .. }
        ));
    }

    #[test]
    fn durable_cancel_restore_requires_exact_typed_origin() {
        let mut user = durable_run("user-child", 1, astra_core::STATUS_CANCELLED);
        user.events.push(json!({
            "event_type": "run_finished",
            "data": {"cancelled": true, "cancellation_origin": "user"}
        }));
        assert!(matches!(
            durable_agent_status(&user),
            AgentStatus::Cancelled { by_user: true, .. }
        ));

        let mut runtime = durable_run("runtime-child", 1, astra_core::STATUS_CANCELLED);
        runtime.events.push(json!({
            "event_type": "run_finished",
            "data": {"cancelled": true, "cancellation_origin": "runtime"}
        }));
        assert!(matches!(
            durable_agent_status(&runtime),
            AgentStatus::Cancelled { by_user: false, .. }
        ));

        let missing = durable_run("missing-origin", 1, astra_core::STATUS_CANCELLED);
        assert!(matches!(
            durable_agent_status(&missing),
            AgentStatus::Interrupted { ref finish_reason, .. }
                if finish_reason == CANCELLATION_ORIGIN_UNVERIFIED
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
    async fn durable_reconciliation_without_typed_origin_interrupts_waiting_child() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        let identity =
            AgentFanoutSlotIdentity::new("review-group", 1, 0, Some("correctness".into())).unwrap();
        spawner
            .record_fanout_spawn_accepted(
                &identity,
                Some("Review"),
                "reviewer-1",
                "child-run",
                "code-review",
                "Review correctness",
                None,
                "root-run",
            )
            .await
            .unwrap();
        let mut waiting = completed_test_state(1);
        waiting.agent_id = "reviewer-1".into();
        waiting.run_id = "child-run".into();
        waiting.parent_run_id = "root-run".into();
        waiting.status = AgentStatus::Waiting {
            reason: "executor_offline".into(),
        };
        waiting.ended_at = None;
        waiting.fanout_slot = Some(identity);
        spawner.archive_state(waiting).await;

        let mut cancelled = durable_run("child-run", 1, astra_core::STATUS_CANCELLED);
        cancelled.agent_id = Some("reviewer-1".into());
        spawner
            .set_durable_agent_reconciler(Arc::new(StaticDurableReconciler {
                runs: vec![cancelled],
            }))
            .await;

        assert_eq!(spawner.reconcile_durable_agent_runs().await.unwrap(), 1);
        let state = spawner
            .get_agent_state_any("reviewer-1")
            .await
            .expect("reconciled child");
        assert!(matches!(
            state.status,
            AgentStatus::Interrupted { ref finish_reason, .. }
                if finish_reason == CANCELLATION_ORIGIN_UNVERIFIED
        ));
        let group = spawner
            .fanout_group_for_agent("reviewer-1")
            .await
            .expect("settled fanout group");
        assert_eq!(group.summary().active, 0);
        assert_eq!(group.summary().interrupted, 1);
        assert_eq!(group.slots[0].status, AgentFanoutSlotStatus::Interrupted);
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
            resolved_model_name: None,
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
            workspace_mutation: Default::default(),
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
            resolved_model_name: None,
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
            workspace_mutation: Default::default(),
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
    async fn spawn_tool_override_is_effective_but_cannot_widen_parent_authority() {
        let executor = Arc::new(CapturingToolBoundsExecutor::new());
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(executor.clone() as Arc<dyn SpawnAgentExecutor>);
        let input = SpawnAgentInput {
            description: "Fetch one artifact".to_string(),
            prompt: "Use the assigned capability".to_string(),
            agent_type: "task".to_string(),
            allowed_tools: Some(vec![
                " settle_work_item ".to_string(),
                "web_fetch".to_string(),
                "web_fetch".to_string(),
            ]),
            run_in_background: false,
            ..Default::default()
        };
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "parent".to_string(),
            resolved_model_name: None,
            recursion_depth: 0,
            parent_is_fork_child: false,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: crate::orchestration::InheritedPermissions {
                allowed_tools: Some(
                    ["bash", "web_fetch"]
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                ),
                ..crate::orchestration::InheritedPermissions::auto_approve()
            },
            inherited_skills: vec![],
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            spawn_tool_call_id: None,
            execution_metadata: None,
            workspace_mutation: Default::default(),
            delegation_chain: Vec::new(),
        };

        assert!(matches!(
            spawner.spawn(input, &context).await.unwrap(),
            SpawnAgentOutput::Completed { .. }
        ));
        let (run_tools, permission_tools) = executor.take_captured().expect("captured tool bounds");
        assert_eq!(
            run_tools,
            vec!["settle_work_item".to_string(), "web_fetch".to_string()]
        );
        assert_eq!(
            permission_tools,
            Some(["web_fetch".to_string()].into_iter().collect()),
            "the child request may narrow its runtime surface but cannot add authority absent from the parent"
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
            resolved_model_name: None,
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
            workspace_mutation: Default::default(),
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

    #[test]
    fn restored_completed_agent_stays_completed_in_progress_projection() {
        let event = agent_status_to_progress_event(
            &AgentStatus::Completed {
                result: "durable result".to_string(),
                finish_reason: Some("restored_from_durable_run".to_string()),
            },
            &SpawnedAgentMetrics {
                tool_calls: 2,
                prompt_tokens: 10,
                completion_tokens: 5,
                ..Default::default()
            },
            SystemTime::now(),
        )
        .expect("restored completed status should emit progress");

        assert!(matches!(
            event,
            ProgressEventType::Completed {
                result_summary,
                total_tool_calls: 2,
                total_tokens: (10, 5),
                ..
            } if result_summary == "durable result"
        ));
    }

    #[test]
    fn completed_status_with_canonical_interruption_reason_stays_interrupted() {
        let event = agent_status_to_progress_event(
            &AgentStatus::Completed {
                result: "partial".to_string(),
                finish_reason: Some("budget_exhausted".to_string()),
            },
            &SpawnedAgentMetrics::default(),
            SystemTime::now(),
        )
        .expect("interrupted completion should emit progress");

        assert!(matches!(
            event,
            ProgressEventType::Interrupted {
                reason,
                partial_summary,
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
            resolved_model_name: None,
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
            workspace_mutation: Default::default(),
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
            resolved_model_name: None,
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
            workspace_mutation: Default::default(),
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
            resolved_model_name: None,
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
            workspace_mutation: Default::default(),
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

    struct GatedBoundedCancellationExecutor {
        current: std::sync::atomic::AtomicUsize,
        maximum: std::sync::atomic::AtomicUsize,
        started: std::sync::atomic::AtomicUsize,
        released: std::sync::atomic::AtomicBool,
        changed: tokio::sync::Notify,
    }

    struct PermanentCancellationFailureExecutor {
        attempts: std::sync::atomic::AtomicUsize,
        attempts_by_run: std::sync::Mutex<HashMap<String, usize>>,
    }

    struct CountingTerminalCancellationExecutor {
        attempts: std::sync::atomic::AtomicUsize,
    }

    struct PanicOnceCancellationExecutor {
        attempts: std::sync::atomic::AtomicUsize,
    }

    struct InvalidCancellationProtocolExecutor {
        attempts: std::sync::atomic::AtomicUsize,
        outcome: SpawnRunCancellationDurability,
    }

    impl GatedBoundedCancellationExecutor {
        fn new() -> Self {
            Self {
                current: std::sync::atomic::AtomicUsize::new(0),
                maximum: std::sync::atomic::AtomicUsize::new(0),
                started: std::sync::atomic::AtomicUsize::new(0),
                released: std::sync::atomic::AtomicBool::new(false),
                changed: tokio::sync::Notify::new(),
            }
        }

        fn release(&self) {
            self.released
                .store(true, std::sync::atomic::Ordering::Release);
            self.changed.notify_waiters();
        }
    }

    struct ImmediateStatusExecutor {
        status: &'static str,
        finish_reason: &'static str,
        output: Option<&'static str>,
        error: Option<&'static str>,
    }

    struct CapturingDepthExecutor {
        captured_depth: std::sync::Mutex<Option<u8>>,
        captured_workspace_mutation:
            std::sync::Mutex<Option<astra_config::user_profile::WorkspaceMutationIntent>>,
    }

    struct CapturingPermissionExecutor {
        captured: std::sync::Mutex<Option<crate::orchestration::permission_sync::PermissionMode>>,
    }

    struct CapturingToolBoundsExecutor {
        captured:
            std::sync::Mutex<Option<(Vec<String>, Option<std::collections::HashSet<String>>)>>,
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
                captured_workspace_mutation: std::sync::Mutex::new(None),
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

    impl CapturingToolBoundsExecutor {
        fn new() -> Self {
            Self {
                captured: std::sync::Mutex::new(None),
            }
        }

        fn take_captured(
            &self,
        ) -> Option<(Vec<String>, Option<std::collections::HashSet<String>>)> {
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
                cancellation_origin: CancellationOrigin::Unverified,
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
                cancellation_origin: CancellationOrigin::Unverified,
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
    impl SpawnAgentExecutor for CapturingToolBoundsExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            *self.captured.lock().unwrap() = Some((
                config.allowed_tools.clone(),
                config.inherited_permissions.allowed_tools.clone(),
            ));
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "completed".into(),
                finish_reason: "normal".into(),
                cancellation_origin: CancellationOrigin::Unverified,
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
                cancellation_origin: CancellationOrigin::Unverified,
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
    impl SpawnAgentExecutor for GatedBoundedCancellationExecutor {
        async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            std::future::pending::<Result<SpawnRunResult, String>>().await
        }

        async fn cancel_spawned_run(
            &self,
            _run_id: &str,
            _cancellation_binding_id: Option<&str>,
            _user_id: Option<&str>,
            _reason: &str,
            _origin: CancellationOrigin,
        ) -> Result<(), String> {
            struct ActiveCall<'a>(&'a std::sync::atomic::AtomicUsize);
            impl Drop for ActiveCall<'_> {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                }
            }

            let current = self
                .current
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
                + 1;
            let _active = ActiveCall(&self.current);
            self.maximum
                .fetch_max(current, std::sync::atomic::Ordering::AcqRel);
            self.started
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            self.changed.notify_waiters();
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.released.load(std::sync::atomic::Ordering::Acquire) {
                    return Ok(());
                }
                changed.await;
            }
        }
    }

    #[async_trait]
    impl SpawnAgentExecutor for PermanentCancellationFailureExecutor {
        async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            std::future::pending::<Result<SpawnRunResult, String>>().await
        }

        async fn cancel_spawned_run(
            &self,
            run_id: &str,
            _cancellation_binding_id: Option<&str>,
            _user_id: Option<&str>,
            _reason: &str,
            _origin: CancellationOrigin,
        ) -> Result<(), String> {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            *self
                .attempts_by_run
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(run_id.to_string())
                .or_default() += 1;
            Err("permanent durable cancellation failure".to_string())
        }
    }

    #[async_trait]
    impl SpawnAgentExecutor for CountingTerminalCancellationExecutor {
        async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            std::future::pending::<Result<SpawnRunResult, String>>().await
        }

        async fn cancel_spawned_run_durably(
            &self,
            _run_id: &str,
            _cancellation_binding_id: Option<&str>,
            _user_id: Option<&str>,
            _reason: &str,
            _origin: CancellationOrigin,
        ) -> Result<SpawnRunCancellationDurability, String> {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            Ok(SpawnRunCancellationDurability::Terminal)
        }
    }

    #[async_trait]
    impl SpawnAgentExecutor for PanicOnceCancellationExecutor {
        async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            std::future::pending::<Result<SpawnRunResult, String>>().await
        }

        async fn cancel_spawned_run_durably(
            &self,
            _run_id: &str,
            _cancellation_binding_id: Option<&str>,
            _user_id: Option<&str>,
            _reason: &str,
            _origin: CancellationOrigin,
        ) -> Result<SpawnRunCancellationDurability, String> {
            let attempt = self
                .attempts
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            if attempt == 0 {
                panic!("injected durable cancellation executor panic");
            }
            Ok(SpawnRunCancellationDurability::Terminal)
        }
    }

    #[async_trait]
    impl SpawnAgentExecutor for InvalidCancellationProtocolExecutor {
        async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            std::future::pending::<Result<SpawnRunResult, String>>().await
        }

        async fn cancel_spawned_run_durably(
            &self,
            _run_id: &str,
            _cancellation_binding_id: Option<&str>,
            _user_id: Option<&str>,
            _reason: &str,
            _origin: CancellationOrigin,
        ) -> Result<SpawnRunCancellationDurability, String> {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            Ok(self.outcome.clone())
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
                cancellation_origin: CancellationOrigin::Unverified,
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
            *self.captured_workspace_mutation.lock().unwrap() = Some(config.workspace_mutation);
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "completed".into(),
                finish_reason: "normal".into(),
                cancellation_origin: CancellationOrigin::Unverified,
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
            cancellation_origin: CancellationOrigin::Unverified,
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

        let empty_completed = SpawnRunResult {
            status: SPAWN_STATUS_COMPLETED.into(),
            finish_reason: "normal".into(),
            output: Some("  ".into()),
            ..interrupted.clone()
        };
        assert!(matches!(
            spawn_run_result_to_agent_status(&empty_completed),
            AgentStatus::Interrupted {
                ref finish_reason,
                ..
            } if finish_reason == AGENT_FINISH_REASON_DURABLE_RESULT_UNAVAILABLE
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
    fn spawn_run_cancellation_origin_projects_to_exact_agent_terminal() {
        let cancelled = SpawnRunResult {
            agent_id: "a1".into(),
            run_id: "r1".into(),
            status: SPAWN_STATUS_CANCELLED.into(),
            finish_reason: "cancelled".into(),
            cancellation_origin: CancellationOrigin::User,
            output: Some("partial child evidence".into()),
            error: Some("user cancelled ancestor run".into()),
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
            turns_completed: 0,
            permission_summary: None,
            permission_requests: 0,
            permission_requests_approved: 0,
            tools_blocked: 0,
        };

        let user_cancelled = spawn_run_result_to_agent_status(&cancelled);
        assert!(matches!(
            user_cancelled,
            AgentStatus::Cancelled {
                by_user: true,
                ref reason,
            } if reason == "user cancelled ancestor run"
        ));
        assert_eq!(
            project_agent_status_to_fanout_slot(&user_cancelled).status,
            AgentFanoutSlotStatus::CancelledByUser,
            "the live terminal and fanout aggregate must share the typed user origin"
        );
        assert!(matches!(
            spawn_run_result_to_sync_output("a1".into(), "r1".into(), cancelled.clone(), 10,),
            SpawnAgentOutput::Cancelled {
                cancelled_by_user: true,
                ..
            }
        ));

        let runtime_cancelled = SpawnRunResult {
            cancellation_origin: CancellationOrigin::Runtime,
            error: Some("fanout deadline".into()),
            ..cancelled.clone()
        };
        let runtime_status = spawn_run_result_to_agent_status(&runtime_cancelled);
        assert!(matches!(
            runtime_status,
            AgentStatus::Cancelled {
                by_user: false,
                ref reason,
            } if reason == "fanout deadline"
        ));
        assert_eq!(
            project_agent_status_to_fanout_slot(&runtime_status).status,
            AgentFanoutSlotStatus::CancelledByRuntime,
            "runtime cancellation must remain distinct from user cancellation"
        );
        assert!(matches!(
            spawn_run_result_to_sync_output("a1".into(), "r1".into(), runtime_cancelled, 10),
            SpawnAgentOutput::Cancelled {
                cancelled_by_user: false,
                ..
            }
        ));

        let unverified = SpawnRunResult {
            cancellation_origin: CancellationOrigin::Unverified,
            error: Some("durable lineage unavailable".into()),
            ..cancelled
        };
        let unverified_status = spawn_run_result_to_agent_status(&unverified);
        assert!(matches!(
            unverified_status,
            AgentStatus::Interrupted {
                ref partial_result,
                ref finish_reason,
            } if partial_result == "partial child evidence"
                && finish_reason == CANCELLATION_ORIGIN_UNVERIFIED
        ));
        assert_eq!(
            project_agent_status_to_fanout_slot(&unverified_status).status,
            AgentFanoutSlotStatus::Interrupted,
            "unverified cancellation must not collapse into either cancellation origin"
        );
        assert!(matches!(
            spawn_run_result_to_sync_output("a1".into(), "r1".into(), unverified, 10),
            SpawnAgentOutput::Interrupted {
                ref result,
                ref finish_reason,
                ..
            } if result == "partial child evidence"
                && finish_reason == CANCELLATION_ORIGIN_UNVERIFIED
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
    fn fanout_slot_status_keeps_child_budget_interruptions_distinct_from_parent_cancel() {
        let budget_interrupted = AgentStatus::Interrupted {
            partial_result: "partial review".to_string(),
            finish_reason: "budget_exhausted".to_string(),
        };
        let projection = project_agent_status_to_fanout_slot(&budget_interrupted);
        assert_eq!(projection.status, AgentFanoutSlotStatus::Interrupted);
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
            cancellation_origin: CancellationOrigin::Unverified,
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
            resolved_model_name: None,
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
            workspace_mutation: Default::default(),
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
            resolved_model_name: None,
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
            workspace_mutation: Default::default(),
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
        assert_eq!(
            *executor.captured_workspace_mutation.lock().unwrap(),
            Some(astra_config::user_profile::WorkspaceMutationIntent::ReadOnly),
            "a role-declared read-only child must narrow an unknown parent boundary"
        );
    }

    #[tokio::test]
    async fn spawn_threads_parent_workspace_mutation_to_capable_child() {
        let executor = Arc::new(CapturingDepthExecutor::new());
        let spawner = DynamicAgentSpawner::new(mock_router()).with_executor(executor.clone());
        let mut context = make_bg_context();
        context.recursion_depth = 0;
        context.workspace_mutation =
            astra_config::user_profile::WorkspaceMutationIntent::MustMutate;
        let input = SpawnAgentInput {
            description: "Implement the admitted change".into(),
            prompt: "Apply and verify the change".into(),
            agent_type: "task".into(),
            run_in_background: false,
            ..Default::default()
        };

        let result = spawner.spawn(input, &context).await.unwrap();
        assert!(matches!(result, SpawnAgentOutput::Completed { .. }));
        assert_eq!(
            *executor.captured_workspace_mutation.lock().unwrap(),
            Some(astra_config::user_profile::WorkspaceMutationIntent::MustMutate),
            "a mutation-capable child must inherit the root completion/effect boundary"
        );
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
            resolved_model_name: None,
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
            workspace_mutation: Default::default(),
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
            resolved_model_name: None,
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
            workspace_mutation: Default::default(),
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
            resolved_model_name: None,
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
            workspace_mutation: Default::default(),
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
        let journal = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let journal = std::fs::read_to_string(&journal_path).unwrap_or_default();
                if journal.contains("\"type\":\"agent_terminated\"") {
                    break journal;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("best-effort terminal journal persistence must settle within its bound");
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
            resolved_model_name: None,
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
            workspace_mutation: Default::default(),
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
            resolved_model_name: None,
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
            workspace_mutation: Default::default(),
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
    async fn fanout_cancel_terminalizes_archived_nonterminal_slot() {
        let spawner = DynamicAgentSpawner::new(mock_router()).with_executor(Arc::new(
            ImmediateStatusExecutor {
                status: SPAWN_STATUS_WAITING,
                finish_reason: "waiting",
                output: Some("executor_offline"),
                error: None,
            },
        ));
        let mut input = make_sync_input();
        input.fanout_group_id = Some("waiting-review".to_string());
        input.fanout_group_title = Some("Waiting review".to_string());
        input.fanout_target_count = Some(1);
        input.fanout_slot_index = Some(0);

        let output = spawner.spawn(input, &make_bg_context()).await.unwrap();
        let agent_id = match output {
            SpawnAgentOutput::Waiting { agent_id, .. } => agent_id,
            other => panic!("expected archived waiting output, got {other:?}"),
        };
        assert!(spawner.active_agents.read().await.is_empty());
        assert!(matches!(
            spawner.get_agent_state_any(&agent_id).await.unwrap().status,
            AgentStatus::Waiting { .. }
        ));

        let cancelled = spawner
            .cancel_fanout_group_for_user("waiting-review", "user stopped group")
            .await
            .expect("fanout group must remain addressable");

        assert_eq!(cancelled.stopped_agent_ids, vec![agent_id.clone()]);
        assert!(cancelled.not_stopped_agent_ids.is_empty());
        assert!(cancelled.group.is_terminal());
        assert!(matches!(
            spawner.get_agent_state_any(&agent_id).await.unwrap().status,
            AgentStatus::Cancelled { by_user: true, .. }
        ));
    }

    #[tokio::test]
    async fn ancestor_cancel_includes_archived_nonterminal_descendants() {
        let spawner = DynamicAgentSpawner::new(mock_router()).with_executor(Arc::new(
            ImmediateStatusExecutor {
                status: SPAWN_STATUS_WAITING,
                finish_reason: "waiting",
                output: Some("executor_offline"),
                error: None,
            },
        ));

        let output = spawner
            .spawn(make_sync_input(), &make_bg_context())
            .await
            .unwrap();
        let agent_id = match output {
            SpawnAgentOutput::Waiting { agent_id, .. } => agent_id,
            other => panic!("expected archived waiting output, got {other:?}"),
        };

        let cancelled = spawner
            .cancel_descendants_of_parent_run_for_user("root")
            .await;

        assert_eq!(cancelled, 1);
        assert!(matches!(
            spawner.get_agent_state_any(&agent_id).await.unwrap().status,
            AgentStatus::Cancelled { by_user: true, .. }
        ));
    }

    #[tokio::test]
    async fn ancestor_user_cancel_seizes_initializing_child_before_handle_installation() {
        let register_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(PendingRegisterTransport {
                register_started: Arc::clone(&register_started),
            }),
            Arc::new(DelegationTracker::new()),
        ));
        let spawner = Arc::new(
            DynamicAgentSpawner::new(router)
                .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>),
        );
        let spawn_task = {
            let spawner = Arc::clone(&spawner);
            tokio::spawn(async move { spawner.spawn(make_bg_input(), &make_bg_context()).await })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while !register_started.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("spawn must reach pre-handle mailbox preparation");
        let active = spawner.active_agents.read().await;
        let agent_id = active
            .values()
            .next()
            .expect("initializing child must own an active reservation")
            .agent_id
            .clone();
        assert!(matches!(
            active.get(&agent_id).map(|state| &state.status),
            Some(AgentStatus::Initializing)
        ));
        drop(active);
        assert!(spawner.background_abort_handles.read().await.is_empty());

        assert_eq!(
            spawner
                .cancel_descendants_of_parent_run_for_user("root")
                .await,
            1
        );
        assert!(spawner.active_agents.read().await.is_empty());
        assert!(matches!(
            spawner.get_agent_state_any(&agent_id).await.unwrap().status,
            AgentStatus::Cancelled { by_user: true, .. }
        ));

        spawn_task.abort();
        assert!(
            spawn_task
                .await
                .expect_err("pending preparation must abort")
                .is_cancelled()
        );
    }

    #[tokio::test]
    async fn exact_runtime_cancellation_starts_all_child_durability_in_one_bound() {
        struct StalledDurableCancellation {
            started: std::sync::atomic::AtomicUsize,
            release: tokio::sync::Notify,
        }

        #[async_trait]
        impl SpawnAgentExecutor for StalledDurableCancellation {
            async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
                std::future::pending::<Result<SpawnRunResult, String>>().await
            }

            async fn cancel_spawned_run(
                &self,
                _run_id: &str,
                _cancellation_binding_id: Option<&str>,
                _user_id: Option<&str>,
                _reason: &str,
                _origin: CancellationOrigin,
            ) -> Result<(), String> {
                self.started
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.release.notified().await;
                Ok(())
            }
        }

        let executor = Arc::new(StalledDurableCancellation {
            started: std::sync::atomic::AtomicUsize::new(0),
            release: tokio::sync::Notify::new(),
        });
        let spawner = Arc::new(
            DynamicAgentSpawner::new(mock_router())
                .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>),
        );
        let mut agent_ids = Vec::new();
        for _ in 0..3 {
            let output = spawner
                .spawn(make_bg_input(), &make_bg_context())
                .await
                .expect("launch child");
            match output {
                SpawnAgentOutput::Launched { agent_id, .. } => agent_ids.push(agent_id),
                other => panic!("expected launched child, got {other:?}"),
            }
        }

        let cancellation = {
            let spawner = Arc::clone(&spawner);
            tokio::spawn(async move {
                futures_util::future::join_all(agent_ids.iter().map(|agent_id| {
                    spawner.cancel_agent_for_runtime(
                        agent_id,
                        "runtime cancelled exact child execution",
                    )
                }))
                .await
                .into_iter()
                .filter(|outcome| outcome.owns_local_stop())
                .count()
            })
        };
        tokio::time::timeout(Duration::from_millis(250), async {
            while executor.started.load(std::sync::atomic::Ordering::SeqCst) < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("every descendant must cross local seizure before durable I/O settles");
        assert!(
            spawner
                .completed_agents
                .read()
                .await
                .iter()
                .all(|state| !matches!(state.status, AgentStatus::Cancelled { .. })),
            "a stalled durable call cannot manufacture runtime cancellation terminals"
        );

        executor.release.notify_waiters();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), cancellation)
                .await
                .expect("descendant cancellation must share one durability bound")
                .expect("cancellation owner must not panic"),
            3
        );
        wait_for_durable_cancellations_to_settle(&spawner).await;
        assert_eq!(
            spawner
                .completed_agents
                .read()
                .await
                .iter()
                .filter(|state| matches!(
                    state.status,
                    AgentStatus::Cancelled { by_user: false, .. }
                ))
                .count(),
            3,
            "durable Terminal is the first runtime cancellation terminal"
        );
    }

    async fn launch_pending_child(
        spawner: &DynamicAgentSpawner,
        parent_run_id: &str,
    ) -> (String, String) {
        let mut context = make_bg_context();
        context.parent_run_id = parent_run_id.to_string();
        match spawner
            .spawn(make_bg_input(), &context)
            .await
            .expect("launch pending child")
        {
            SpawnAgentOutput::Launched {
                agent_id, run_id, ..
            } => (agent_id, run_id),
            other => panic!("expected launched child, got {other:?}"),
        }
    }

    async fn wait_for_durable_cancellations_to_settle(spawner: &DynamicAgentSpawner) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while spawner.has_in_flight_cancellation_owners().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("durable cancellation scheduler must converge");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn hundreds_of_descendants_seize_locally_before_bounded_durability() {
        const DESCENDANTS: usize = 257;
        let executor = Arc::new(GatedBoundedCancellationExecutor::new());
        let spawner = Arc::new(
            DynamicAgentSpawner::new(mock_router())
                .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>),
        );
        for _ in 0..DESCENDANTS {
            let _ = launch_pending_child(&spawner, ROOT_RUN_ID).await;
        }

        let cancelled = tokio::time::timeout(
            Duration::from_secs(2),
            spawner.cancel_descendants_of_parent_run_for_user(ROOT_RUN_ID),
        )
        .await
        .expect("local seizure must not scale with durable timeout waves");
        assert_eq!(cancelled, DESCENDANTS);
        assert!(spawner.active_agents.read().await.is_empty());
        assert!(spawner.background_abort_handles.read().await.is_empty());
        assert_eq!(
            spawner.in_flight_cancellations.read().await.len(),
            DESCENDANTS
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            while executor.started.load(std::sync::atomic::Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("durable scheduler must start after local seizure");
        assert!(
            executor.maximum.load(std::sync::atomic::Ordering::Acquire)
                <= CANCELLATION_RETRY_PER_SPAWNER_CONCURRENCY,
            "one session must never flood the shared durable backend"
        );

        executor.release();
        wait_for_durable_cancellations_to_settle(&spawner).await;
        assert!(
            spawner
                .completed_agents
                .read()
                .await
                .iter()
                .all(|state| matches!(state.status, AgentStatus::Cancelled { .. }))
        );
        assert!(
            executor.maximum.load(std::sync::atomic::Ordering::Acquire)
                <= CANCELLATION_RETRY_PER_SPAWNER_CONCURRENCY
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn durable_cancellation_scheduler_gives_independent_sessions_progress() {
        let first_executor = Arc::new(GatedBoundedCancellationExecutor::new());
        let second_executor = Arc::new(GatedBoundedCancellationExecutor::new());
        let first = Arc::new(
            DynamicAgentSpawner::new(mock_router())
                .with_executor(Arc::clone(&first_executor) as Arc<dyn SpawnAgentExecutor>),
        );
        let second = Arc::new(
            DynamicAgentSpawner::new(mock_router())
                .with_executor(Arc::clone(&second_executor) as Arc<dyn SpawnAgentExecutor>),
        );
        let shared_capacity = Arc::new(tokio::sync::Semaphore::new(1));
        first.set_cancellation_capacity_for_test(Arc::clone(&shared_capacity));
        second.set_cancellation_capacity_for_test(shared_capacity);
        let second_waiting = Arc::new(tokio::sync::Notify::new());
        second.set_cancellation_capacity_waiting_hook_for_test(Some(Arc::clone(&second_waiting)));
        for _ in 0..12 {
            let _ = launch_pending_child(&first, ROOT_RUN_ID).await;
        }
        let _ = launch_pending_child(&second, ROOT_RUN_ID).await;

        assert_eq!(
            first
                .cancel_descendants_of_parent_run_for_user(ROOT_RUN_ID)
                .await,
            12
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while first_executor
                .started
                .load(std::sync::atomic::Ordering::Acquire)
                == 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("large session must occupy its bounded wave");

        assert_eq!(
            second
                .cancel_descendants_of_parent_run_for_user(ROOT_RUN_ID)
                .await,
            1
        );
        tokio::time::timeout(Duration::from_secs(1), second_waiting.notified())
            .await
            .expect("later session must reach the shared capacity queue");
        assert_eq!(
            second_executor
                .started
                .load(std::sync::atomic::Ordering::Acquire),
            0,
            "the injected single permit is still held by the first session"
        );
        first_executor.release();
        tokio::time::timeout(Duration::from_secs(1), async {
            while second_executor
                .started
                .load(std::sync::atomic::Ordering::Acquire)
                == 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a later session must make bounded progress after one local wave");
        assert!(
            first_executor
                .started
                .load(std::sync::atomic::Ordering::Acquire)
                <= CANCELLATION_RETRY_PER_SPAWNER_CONCURRENCY,
            "one spawner may queue only its bounded local wave ahead of a later session"
        );

        second_executor.release();
        wait_for_durable_cancellations_to_settle(&first).await;
        wait_for_durable_cancellations_to_settle(&second).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_batch_caller_cannot_lose_the_unvisited_suffix() {
        let executor = Arc::new(GatedBoundedCancellationExecutor::new());
        let spawner = Arc::new(
            DynamicAgentSpawner::new(mock_router())
                .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>),
        );
        for _ in 0..7 {
            let _ = launch_pending_child(&spawner, ROOT_RUN_ID).await;
        }
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        spawner.set_cancellation_before_in_flight_hook(Some((
            Arc::clone(&entered),
            Arc::clone(&release),
        )));

        let caller = {
            let spawner = Arc::clone(&spawner);
            tokio::spawn(async move {
                spawner
                    .cancel_descendants_of_parent_run_for_user(ROOT_RUN_ID)
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("first child must cross the ownership-transfer gate");
        caller.abort();
        assert!(
            caller
                .await
                .expect_err("caller must be aborted")
                .is_cancelled()
        );
        spawner.set_cancellation_before_in_flight_hook(None);
        release.notify_one();

        tokio::time::timeout(Duration::from_secs(1), async {
            while !spawner.active_agents.read().await.is_empty()
                || !spawner.background_abort_handles.read().await.is_empty()
                || spawner.in_flight_cancellations.read().await.len() != 7
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached local owner must seize every remaining child");

        executor.release();
        wait_for_durable_cancellations_to_settle(&spawner).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn direct_user_cancel_seizes_entire_subtree_before_target_terminal_cas() {
        struct TargetTerminalGate {
            target_run_id: std::sync::Mutex<Option<String>>,
            attempted_run_ids: std::sync::Mutex<Vec<String>>,
            target_entered: tokio::sync::Notify,
            released: std::sync::atomic::AtomicBool,
            changed: tokio::sync::Notify,
        }

        #[async_trait]
        impl SpawnAgentExecutor for TargetTerminalGate {
            async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
                std::future::pending::<Result<SpawnRunResult, String>>().await
            }

            async fn cancel_spawned_run(
                &self,
                run_id: &str,
                _cancellation_binding_id: Option<&str>,
                _user_id: Option<&str>,
                _reason: &str,
                _origin: CancellationOrigin,
            ) -> Result<(), String> {
                self.attempted_run_ids
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(run_id.to_string());
                if self
                    .target_run_id
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_deref()
                    == Some(run_id)
                {
                    self.target_entered.notify_one();
                }
                loop {
                    let changed = self.changed.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    if self.released.load(std::sync::atomic::Ordering::Acquire) {
                        return Ok(());
                    }
                    changed.await;
                }
            }
        }

        let executor = Arc::new(TargetTerminalGate {
            target_run_id: std::sync::Mutex::new(None),
            attempted_run_ids: std::sync::Mutex::new(Vec::new()),
            target_entered: tokio::sync::Notify::new(),
            released: std::sync::atomic::AtomicBool::new(false),
            changed: tokio::sync::Notify::new(),
        });
        let spawner = Arc::new(
            DynamicAgentSpawner::new(mock_router())
                .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>),
        );
        let (target_agent_id, target_run_id) = launch_pending_child(&spawner, ROOT_RUN_ID).await;
        let (_, child_run_id) = launch_pending_child(&spawner, &target_run_id).await;
        let _ = launch_pending_child(&spawner, &child_run_id).await;
        *executor
            .target_run_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(target_run_id);

        let seizure_entered = Arc::new(tokio::sync::Notify::new());
        let seizure_release = Arc::new(tokio::sync::Notify::new());
        spawner.set_cancellation_before_in_flight_hook(Some((
            Arc::clone(&seizure_entered),
            Arc::clone(&seizure_release),
        )));
        let caller = {
            let spawner = Arc::clone(&spawner);
            let target_agent_id = target_agent_id.clone();
            tokio::spawn(async move {
                spawner
                    .cancel_agent_for_user(&target_agent_id, "stop the whole delegated subtree")
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(1), seizure_entered.notified())
            .await
            .expect("deepest descendant must enter local seizure");
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        spawner.set_cancellation_before_in_flight_hook(None);
        seizure_release.notify_one();

        tokio::time::timeout(Duration::from_secs(1), async {
            while !spawner.active_agents.read().await.is_empty()
                || !spawner.background_abort_handles.read().await.is_empty()
                || spawner.in_flight_cancellations.read().await.len() != 3
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached subtree owner must seize target and all descendants");
        assert!(
            tokio::time::timeout(Duration::from_secs(1), executor.target_entered.notified())
                .await
                .is_ok(),
            "target terminal CAS must start after subtree seizure; attempts={:?}",
            executor
                .attempted_run_ids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        );
        assert!(spawner.active_agents.read().await.is_empty());
        assert!(spawner.background_abort_handles.read().await.is_empty());
        assert_eq!(
            spawner.in_flight_cancellations.read().await.len(),
            3,
            "target and both descendants must retain exact pending owners"
        );

        executor
            .released
            .store(true, std::sync::atomic::Ordering::Release);
        executor.changed.notify_waiters();
        wait_for_durable_cancellations_to_settle(&spawner).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ancestor_user_cancel_traverses_runtime_owner_during_active_archive_handoff() {
        struct GatedUserTerminalExecutor {
            user_attempted: tokio::sync::Notify,
            released: std::sync::atomic::AtomicBool,
            changed: tokio::sync::Notify,
        }

        #[async_trait]
        impl SpawnAgentExecutor for GatedUserTerminalExecutor {
            async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
                std::future::pending::<Result<SpawnRunResult, String>>().await
            }

            async fn cancel_spawned_run_durably(
                &self,
                _run_id: &str,
                _cancellation_binding_id: Option<&str>,
                _user_id: Option<&str>,
                _reason: &str,
                origin: CancellationOrigin,
            ) -> Result<SpawnRunCancellationDurability, String> {
                if origin == CancellationOrigin::User {
                    self.user_attempted.notify_one();
                }
                loop {
                    let changed = self.changed.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    if self.released.load(std::sync::atomic::Ordering::Acquire) {
                        return Ok(SpawnRunCancellationDurability::Terminal);
                    }
                    changed.await;
                }
            }
        }

        let executor = Arc::new(GatedUserTerminalExecutor {
            user_attempted: tokio::sync::Notify::new(),
            released: std::sync::atomic::AtomicBool::new(false),
            changed: tokio::sync::Notify::new(),
        });
        let spawner = Arc::new(
            DynamicAgentSpawner::new(mock_router())
                .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>),
        );
        let (ancestor_agent_id, ancestor_run_id) =
            launch_pending_child(&spawner, ROOT_RUN_ID).await;
        let ancestor_binding = spawner
            .active_agents
            .read()
            .await
            .get(&ancestor_agent_id)
            .and_then(|state| state.cancellation_binding_id.clone());
        let (_, child_run_id) = launch_pending_child(&spawner, &ancestor_run_id).await;
        let (grandchild_agent_id, _) = launch_pending_child(&spawner, &child_run_id).await;

        let handoff_entered = Arc::new(tokio::sync::Notify::new());
        let handoff_release = Arc::new(tokio::sync::Notify::new());
        spawner.set_cancellation_before_in_flight_hook(Some((
            Arc::clone(&handoff_entered),
            Arc::clone(&handoff_release),
        )));
        let runtime_caller = {
            let spawner = Arc::clone(&spawner);
            let ancestor_agent_id = ancestor_agent_id.clone();
            tokio::spawn(async move {
                spawner
                    .cancel_agent_for_runtime(&ancestor_agent_id, "runtime deadline")
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(1), handoff_entered.notified())
            .await
            .expect("runtime cancellation must enter active-to-archive handoff");
        spawner.set_cancellation_before_in_flight_hook(None);

        assert!(
            spawner
                .completed_agents
                .read()
                .await
                .iter()
                .all(|state| state.agent_id != ancestor_agent_id),
            "the exact runtime owner must supply lineage before archive publication"
        );
        assert_eq!(
            spawner
                .cancel_descendants_of_parent_run_for_user(ROOT_RUN_ID)
                .await,
            2,
            "the already-pending ancestor is upgraded while both live descendants are seized"
        );
        tokio::time::timeout(Duration::from_secs(1), executor.user_attempted.notified())
            .await
            .expect("upgraded User durable operation must be admitted");

        assert!(
            !spawner
                .active_agents
                .read()
                .await
                .contains_key(&grandchild_agent_id),
            "grandchild execution must stop before an ancestor terminal can be acknowledged"
        );
        assert!(
            !spawner
                .background_abort_handles
                .read()
                .await
                .contains_key(&grandchild_agent_id)
        );
        let pending = spawner.in_flight_cancellations.read().await;
        assert_eq!(pending.len(), 3);
        let ancestor_owner = pending
            .get(&ancestor_agent_id)
            .expect("runtime owner remains exact while archive publication is gated");
        assert_eq!(ancestor_owner.origin, CancellationOrigin::User);
        assert_eq!(ancestor_owner.parent_run_id, ROOT_RUN_ID);
        assert_eq!(ancestor_owner.run_id, ancestor_run_id);
        assert_eq!(ancestor_owner.cancellation_binding_id, ancestor_binding);
        assert_eq!(ancestor_owner.owner_version, 2);
        drop(pending);

        handoff_release.notify_one();
        assert!(matches!(
            runtime_caller.await.expect("runtime caller task"),
            CancellationTransferOutcome::SeizedPending
        ));
        executor
            .released
            .store(true, std::sync::atomic::Ordering::Release);
        executor.changed.notify_waiters();
        wait_for_durable_cancellations_to_settle(&spawner).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn terminal_archived_intermediate_connects_live_grandchild_without_being_seized() {
        let executor = Arc::new(GatedBoundedCancellationExecutor::new());
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>);
        let (child_agent_id, child_run_id) = launch_pending_child(&spawner, ROOT_RUN_ID).await;
        let (grandchild_agent_id, _) = launch_pending_child(&spawner, &child_run_id).await;

        if let Some(handle) = spawner
            .background_abort_handles
            .write()
            .await
            .remove(&child_agent_id)
        {
            handle.abort();
        }
        let mut completed_child = spawner
            .active_agents
            .write()
            .await
            .remove(&child_agent_id)
            .expect("child is active before terminal publication");
        completed_child.status = AgentStatus::Completed {
            result: "child completed while grandchild continues".to_string(),
            finish_reason: Some("normal".to_string()),
        };
        completed_child.work_revision = completed_child.work_revision.saturating_add(1);
        completed_child.ended_at = Some(SystemTime::now());
        spawner.archive_state(completed_child.clone()).await;

        assert_eq!(
            spawner
                .cancel_descendants_of_parent_run_for_user(ROOT_RUN_ID)
                .await,
            1,
            "only the live grandchild is a seizure target"
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while executor.started.load(std::sync::atomic::Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("grandchild durable terminal gate must be reached");
        assert!(
            !spawner
                .active_agents
                .read()
                .await
                .contains_key(&grandchild_agent_id),
            "grandchild must be locally seized before terminal acknowledgement"
        );
        let pending = spawner.in_flight_cancellations.read().await;
        assert!(pending.contains_key(&grandchild_agent_id));
        assert!(
            !pending.contains_key(&child_agent_id),
            "terminal connector must never become a cancellation owner"
        );
        drop(pending);
        let archived_child = spawner
            .completed_agents
            .read()
            .await
            .iter()
            .rev()
            .find(|state| state.agent_id == child_agent_id)
            .cloned()
            .expect("terminal child remains archived");
        assert_eq!(archived_child.status, completed_child.status);
        assert_eq!(archived_child.work_revision, completed_child.work_revision);

        executor.release();
        wait_for_durable_cancellations_to_settle(&spawner).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn coherent_snapshot_keeps_pending_intermediate_reachable_exactly_once() {
        struct RecordingGateExecutor {
            calls: std::sync::Mutex<Vec<(String, CancellationOrigin)>>,
            released: std::sync::atomic::AtomicBool,
            changed: tokio::sync::Notify,
        }

        #[async_trait]
        impl SpawnAgentExecutor for RecordingGateExecutor {
            async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
                std::future::pending::<Result<SpawnRunResult, String>>().await
            }

            async fn cancel_spawned_run_durably(
                &self,
                run_id: &str,
                _cancellation_binding_id: Option<&str>,
                _user_id: Option<&str>,
                _reason: &str,
                origin: CancellationOrigin,
            ) -> Result<SpawnRunCancellationDurability, String> {
                self.calls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((run_id.to_string(), origin));
                loop {
                    let changed = self.changed.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    if self.released.load(std::sync::atomic::Ordering::Acquire) {
                        return Ok(SpawnRunCancellationDurability::Terminal);
                    }
                    changed.await;
                }
            }
        }

        let executor = Arc::new(RecordingGateExecutor {
            calls: std::sync::Mutex::new(Vec::new()),
            released: std::sync::atomic::AtomicBool::new(false),
            changed: tokio::sync::Notify::new(),
        });
        let spawner = Arc::new(
            DynamicAgentSpawner::new(mock_router())
                .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>),
        );
        let (intermediate_agent_id, intermediate_run_id) =
            launch_pending_child(&spawner, ROOT_RUN_ID).await;
        let (grandchild_agent_id, grandchild_run_id) =
            launch_pending_child(&spawner, &intermediate_run_id).await;

        let handoff_entered = Arc::new(tokio::sync::Notify::new());
        let handoff_release = Arc::new(tokio::sync::Notify::new());
        spawner.set_cancellation_before_in_flight_hook(Some((
            Arc::clone(&handoff_entered),
            Arc::clone(&handoff_release),
        )));
        let runtime_caller = {
            let spawner = Arc::clone(&spawner);
            let intermediate_agent_id = intermediate_agent_id.clone();
            tokio::spawn(async move {
                spawner
                    .cancel_agent_for_runtime(&intermediate_agent_id, "runtime deadline")
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(1), handoff_entered.notified())
            .await
            .expect("intermediate enters active-to-archive gap");
        spawner.set_cancellation_before_in_flight_hook(None);

        let snapshot_entered = Arc::new(tokio::sync::Notify::new());
        let snapshot_release = Arc::new(tokio::sync::Notify::new());
        spawner.set_descendant_snapshot_before_pending_hook(Some((
            Arc::clone(&snapshot_entered),
            Arc::clone(&snapshot_release),
        )));
        let ancestor_cancel = {
            let spawner = Arc::clone(&spawner);
            tokio::spawn(async move {
                spawner
                    .cancel_descendants_of_parent_run_for_user(ROOT_RUN_ID)
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(1), snapshot_entered.notified())
            .await
            .expect("snapshot must finish active and archive collections");

        // Move the intermediate into the archive only after that collection
        // was sampled. Its exact in-flight owner cannot be admitted/finalized
        // while the coherent snapshot retains the read guard.
        handoff_release.notify_one();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !spawner.completed_agents.read().await.iter().any(|state| {
                state.agent_id == intermediate_agent_id
                    && matches!(state.status, AgentStatus::Waiting { .. })
            }) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("pending intermediate must archive between snapshot collections");
        assert!(
            executor
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "in-flight read guard must block terminal admission until snapshot consumes lineage"
        );

        spawner.set_descendant_snapshot_before_pending_hook(None);
        snapshot_release.notify_one();
        assert_eq!(
            ancestor_cancel.await.expect("ancestor cancellation task"),
            1,
            "the live grandchild is seized and pending intermediate upgraded"
        );
        assert!(matches!(
            runtime_caller.await.expect("runtime cancellation task"),
            CancellationTransferOutcome::SeizedPending
        ));
        let pending = spawner.in_flight_cancellations.read().await;
        assert_eq!(pending.len(), 2);
        assert_eq!(
            pending
                .get(&intermediate_agent_id)
                .expect("intermediate remains reachable")
                .owner_version,
            2,
            "one traversal performs one Runtime-to-User owner upgrade"
        );
        assert!(pending.contains_key(&grandchild_agent_id));
        drop(pending);

        executor
            .released
            .store(true, std::sync::atomic::Ordering::Release);
        executor.changed.notify_waiters();
        wait_for_durable_cancellations_to_settle(&spawner).await;
        let calls = executor
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            calls
                .iter()
                .filter(|(run_id, origin)| {
                    run_id == &intermediate_run_id && *origin == CancellationOrigin::User
                })
                .count(),
            1,
            "intermediate connector is consumed exactly once"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|(run_id, origin)| {
                    run_id == &grandchild_run_id && *origin == CancellationOrigin::User
                })
                .count(),
            1,
            "grandchild is seized exactly once"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn user_upgrade_cancels_stale_runtime_attempt_and_publishes_one_user_terminal() {
        #[derive(Clone)]
        struct CancellationCall {
            origin: CancellationOrigin,
            reason: String,
            binding: Option<String>,
        }

        struct RuntimeUpgradeExecutor {
            calls: std::sync::Mutex<Vec<CancellationCall>>,
            runtime_attempted: tokio::sync::Notify,
            runtime_release: tokio::sync::Notify,
            runtime_returned: std::sync::atomic::AtomicBool,
        }

        #[async_trait]
        impl SpawnAgentExecutor for RuntimeUpgradeExecutor {
            async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
                std::future::pending::<Result<SpawnRunResult, String>>().await
            }

            async fn cancel_spawned_run_durably(
                &self,
                _run_id: &str,
                cancellation_binding_id: Option<&str>,
                _user_id: Option<&str>,
                reason: &str,
                origin: CancellationOrigin,
            ) -> Result<SpawnRunCancellationDurability, String> {
                self.calls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(CancellationCall {
                        origin,
                        reason: reason.to_string(),
                        binding: cancellation_binding_id.map(ToString::to_string),
                    });
                if origin == CancellationOrigin::Runtime {
                    self.runtime_attempted.notify_one();
                    self.runtime_release.notified().await;
                    self.runtime_returned
                        .store(true, std::sync::atomic::Ordering::Release);
                }
                Ok(SpawnRunCancellationDurability::Terminal)
            }
        }

        let journal_dir = tempfile::TempDir::new().expect("create journal directory");
        let _journal_guard =
            astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let executor = Arc::new(RuntimeUpgradeExecutor {
            calls: std::sync::Mutex::new(Vec::new()),
            runtime_attempted: tokio::sync::Notify::new(),
            runtime_release: tokio::sync::Notify::new(),
            runtime_returned: std::sync::atomic::AtomicBool::new(false),
        });
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_session("runtime-to-user-upgrade-session".to_string())
            .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>);
        let group_id = "runtime-to-user-upgrade";
        let agent_id = spawn_single_slot_fanout(&spawner, group_id).await;
        let binding = spawner
            .active_agents
            .read()
            .await
            .get(&agent_id)
            .and_then(|state| state.cancellation_binding_id.clone());

        let runtime = spawner
            .cancel_fanout_group_for_runtime(group_id, "runtime deadline")
            .await
            .expect("fanout remains queryable");
        assert_eq!(
            runtime.cancellation_pending_agent_ids,
            std::slice::from_ref(&agent_id)
        );
        tokio::time::timeout(
            Duration::from_secs(1),
            executor.runtime_attempted.notified(),
        )
        .await
        .expect("runtime durable attempt must stall after cloning its owner");

        assert_eq!(
            spawner
                .cancel_agent_for_user(&agent_id, "user explicitly stopped agent")
                .await,
            CancellationTransferOutcome::AlreadyPending
        );
        executor.runtime_release.notify_waiters();
        wait_for_durable_cancellations_to_settle(&spawner).await;

        assert!(
            !executor
                .runtime_returned
                .load(std::sync::atomic::Ordering::Acquire),
            "owner upgrade must drop the cloned Runtime future before it can return Terminal"
        );
        let calls = executor
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(calls.len(), 2, "exactly one Runtime and one User operation");
        assert_eq!(calls[0].origin, CancellationOrigin::Runtime);
        assert_eq!(calls[0].reason, "runtime deadline");
        assert_eq!(calls[0].binding, binding);
        assert_eq!(calls[1].origin, CancellationOrigin::User);
        assert_eq!(calls[1].reason, "user explicitly stopped agent");
        assert_eq!(calls[1].binding, binding);

        let state = spawner
            .get_agent_state_any(&agent_id)
            .await
            .expect("terminal child remains queryable");
        assert!(matches!(
            state.status,
            AgentStatus::Cancelled { by_user: true, ref reason }
                if reason == "user explicitly stopped agent"
        ));
        let group = spawner
            .fanout_group(group_id)
            .await
            .expect("terminal fanout remains queryable");
        assert_eq!(
            group.slots[0].status,
            AgentFanoutSlotStatus::CancelledByUser
        );
        assert_eq!(group.summary().cancelled_by_user, 1);
        let terminal_event_count = || {
            astra_services::session_journal::read_journal("runtime-to-user-upgrade-session")
                .expect("read terminal journal")
                .into_iter()
                .filter(|event| {
                    event.event_type
                        == astra_services::session_journal::JournalEventType::AgentTerminated
                })
                .count()
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while terminal_event_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("authoritative User terminal journal must converge");
        tokio::time::sleep(Duration::from_millis(20)).await;
        let terminal_events = terminal_event_count();
        assert_eq!(terminal_events, 1, "canonical terminal must be exact-once");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn odd_multilevel_descendant_tree_is_not_skipped() {
        let executor = Arc::new(GatedBoundedCancellationExecutor::new());
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>);
        let (_, first_root_run) = launch_pending_child(&spawner, ROOT_RUN_ID).await;
        let (_, second_root_run) = launch_pending_child(&spawner, ROOT_RUN_ID).await;
        let (_, first_child_run) = launch_pending_child(&spawner, &first_root_run).await;
        let _ = launch_pending_child(&spawner, &first_root_run).await;
        let _ = launch_pending_child(&spawner, &first_child_run).await;
        let (_, second_child_run) = launch_pending_child(&spawner, &second_root_run).await;
        let _ = launch_pending_child(&spawner, &second_child_run).await;

        assert_eq!(
            spawner
                .cancel_descendants_of_parent_run_for_user(ROOT_RUN_ID)
                .await,
            7
        );
        assert!(spawner.active_agents.read().await.is_empty());
        assert!(spawner.background_abort_handles.read().await.is_empty());
        assert_eq!(spawner.in_flight_cancellations.read().await.len(), 7);

        executor.release();
        wait_for_durable_cancellations_to_settle(&spawner).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_same_agent_cancellation_has_one_exact_owner() {
        let executor = Arc::new(GatedBoundedCancellationExecutor::new());
        let spawner = Arc::new(
            DynamicAgentSpawner::new(mock_router())
                .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>),
        );
        let (agent_id, _) = launch_pending_child(&spawner, ROOT_RUN_ID).await;
        let first = {
            let spawner = Arc::clone(&spawner);
            let agent_id = agent_id.clone();
            tokio::spawn(async move {
                spawner
                    .cancel_agent_for_user(&agent_id, "first exact cancellation")
                    .await
            })
        };
        let second = {
            let spawner = Arc::clone(&spawner);
            let agent_id = agent_id.clone();
            tokio::spawn(async move {
                spawner
                    .cancel_agent_for_user(&agent_id, "second exact cancellation")
                    .await
            })
        };
        let (first, second) = tokio::join!(first, second);
        assert_ne!(first.expect("first caller"), second.expect("second caller"));
        assert_eq!(spawner.in_flight_cancellations.read().await.len(), 1);
        assert!(spawner.active_agents.read().await.is_empty());

        executor.release();
        wait_for_durable_cancellations_to_settle(&spawner).await;
        assert_eq!(
            executor.started.load(std::sync::atomic::Ordering::Acquire),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn permanent_failure_supervisor_releases_executor_and_state_after_root_drop() {
        let executor = Arc::new(PermanentCancellationFailureExecutor {
            attempts: std::sync::atomic::AtomicUsize::new(0),
            attempts_by_run: std::sync::Mutex::new(HashMap::new()),
        });
        let weak_executor = Arc::downgrade(&executor);
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>);
        let weak_state = Arc::downgrade(&spawner.in_flight_cancellations);
        let (agent_id, _) = launch_pending_child(&spawner, ROOT_RUN_ID).await;
        assert_eq!(
            spawner
                .cancel_agent_for_user(&agent_id, "permanent backend failure")
                .await,
            CancellationTransferOutcome::SeizedPending
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while executor.attempts.load(std::sync::atomic::Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the first durable failure must be observed");

        drop(executor);
        drop(spawner);
        tokio::time::timeout(Duration::from_secs(1), async {
            while weak_executor.upgrade().is_some() || weak_state.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shutdown must release the retry supervisor's strong session ownership");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn permit_wait_observes_shutdown_and_releases_supervisor() {
        let executor = Arc::new(PermanentCancellationFailureExecutor {
            attempts: std::sync::atomic::AtomicUsize::new(0),
            attempts_by_run: std::sync::Mutex::new(HashMap::new()),
        });
        let weak_executor = Arc::downgrade(&executor);
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>);
        spawner.set_cancellation_capacity_for_test(Arc::new(tokio::sync::Semaphore::new(0)));
        let weak_state = Arc::downgrade(&spawner.in_flight_cancellations);
        let (agent_id, _) = launch_pending_child(&spawner, ROOT_RUN_ID).await;
        assert_eq!(
            spawner
                .cancel_agent_for_runtime(&agent_id, "capacity is exhausted")
                .await,
            CancellationTransferOutcome::SeizedPending
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while !spawner
                .cancellation_retry_supervisor_running
                .load(std::sync::atomic::Ordering::Acquire)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("supervisor must reach the zero-capacity permit wait");
        assert_eq!(
            executor.attempts.load(std::sync::atomic::Ordering::Acquire),
            0
        );

        drop(executor);
        drop(spawner);
        tokio::time::timeout(Duration::from_secs(1), async {
            while weak_executor.upgrade().is_some() || weak_state.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("permit wait must be interrupted by root shutdown");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn panic_after_dequeue_requeues_exact_owner_and_keeps_session_progressing() {
        let executor = Arc::new(CountingTerminalCancellationExecutor {
            attempts: std::sync::atomic::AtomicUsize::new(0),
        });
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>);
        spawner
            .cancellation_retry_panic_after_dequeue
            .store(true, std::sync::atomic::Ordering::Release);

        let (first_agent_id, _) = launch_pending_child(&spawner, ROOT_RUN_ID).await;
        assert_eq!(
            spawner
                .cancel_agent_for_user(&first_agent_id, "panic after ready-index dequeue")
                .await,
            CancellationTransferOutcome::SeizedPending
        );
        wait_for_durable_cancellations_to_settle(&spawner).await;
        assert_eq!(
            executor.attempts.load(std::sync::atomic::Ordering::Acquire),
            1,
            "the unwind guard must retry the popped binding exactly once"
        );

        let (next_agent_id, _) = launch_pending_child(&spawner, ROOT_RUN_ID).await;
        assert_eq!(
            spawner
                .cancel_agent_for_runtime(&next_agent_id, "same-session cancellation after panic")
                .await,
            CancellationTransferOutcome::SeizedPending
        );
        wait_for_durable_cancellations_to_settle(&spawner).await;
        assert_eq!(
            executor.attempts.load(std::sync::atomic::Ordering::Acquire),
            2,
            "clearing the running fence must let later same-session work converge"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn executor_panic_requeues_exact_owner_and_keeps_session_progressing() {
        let executor = Arc::new(PanicOnceCancellationExecutor {
            attempts: std::sync::atomic::AtomicUsize::new(0),
        });
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>);

        let (first_agent_id, _) = launch_pending_child(&spawner, ROOT_RUN_ID).await;
        assert_eq!(
            spawner
                .cancel_agent_for_user(&first_agent_id, "executor panic")
                .await,
            CancellationTransferOutcome::SeizedPending
        );
        wait_for_durable_cancellations_to_settle(&spawner).await;
        assert_eq!(
            executor.attempts.load(std::sync::atomic::Ordering::Acquire),
            2,
            "the panicking attempt must remain owned and retry to authority"
        );

        let (next_agent_id, _) = launch_pending_child(&spawner, ROOT_RUN_ID).await;
        assert_eq!(
            spawner
                .cancel_agent_for_runtime(&next_agent_id, "later same-session cancellation")
                .await,
            CancellationTransferOutcome::SeizedPending
        );
        wait_for_durable_cancellations_to_settle(&spawner).await;
        assert_eq!(
            executor.attempts.load(std::sync::atomic::Ordering::Acquire),
            3,
            "a prior executor panic must not strand the supervisor running fence"
        );
    }

    #[tokio::test]
    async fn new_jobs_cannot_bypass_an_older_failed_jobs_backoff() {
        let executor = Arc::new(PermanentCancellationFailureExecutor {
            attempts: std::sync::atomic::AtomicUsize::new(0),
            attempts_by_run: std::sync::Mutex::new(HashMap::new()),
        });
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>);
        let (first_agent_id, first_run_id) = launch_pending_child(&spawner, ROOT_RUN_ID).await;
        assert_eq!(
            spawner
                .cancel_agent_for_user(&first_agent_id, "first permanent failure")
                .await,
            CancellationTransferOutcome::SeizedPending
        );
        while executor
            .attempts_by_run
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&first_run_id)
            .copied()
            .unwrap_or(0)
            == 0
        {
            tokio::task::yield_now().await;
        }

        for _ in 0..12 {
            let (agent_id, _) = launch_pending_child(&spawner, ROOT_RUN_ID).await;
            assert_eq!(
                spawner
                    .cancel_agent_for_user(&agent_id, "new cancellation during backoff")
                    .await,
                CancellationTransferOutcome::SeizedPending
            );
        }
        let retried_early = tokio::time::timeout(CANCELLATION_RETRY_INITIAL_DELAY / 2, async {
            loop {
                let attempts = executor
                    .attempts_by_run
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&first_run_id)
                    .copied()
                    .unwrap_or(0);
                if attempts >= 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            retried_early.is_err(),
            "new-job wakeups must not retry the failed job before its own deadline"
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let attempts = executor
                    .attempts_by_run
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&first_run_id)
                    .copied()
                    .unwrap_or(0);
                if attempts >= 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the old failed job must retry once its own deadline arrives");
    }

    #[tokio::test]
    async fn invalid_durable_variants_preserve_pending_owner_and_projection() {
        let invalid = [
            SpawnRunCancellationDurability::Superseded(AgentStatus::Running {
                activity: "invalid active superseded status".to_string(),
            }),
            SpawnRunCancellationDurability::NotOwned(AgentStatus::Completed {
                result: "invalid terminal not-owned status".to_string(),
                finish_reason: Some("invalid_protocol".to_string()),
            }),
        ];
        for outcome in invalid {
            let executor = Arc::new(InvalidCancellationProtocolExecutor {
                attempts: std::sync::atomic::AtomicUsize::new(0),
                outcome,
            });
            let spawner = DynamicAgentSpawner::new(mock_router())
                .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>);
            let (agent_id, _) = launch_pending_child(&spawner, ROOT_RUN_ID).await;
            assert_eq!(
                spawner
                    .cancel_agent_for_runtime(&agent_id, "invalid durable protocol")
                    .await,
                CancellationTransferOutcome::SeizedPending
            );
            tokio::time::timeout(Duration::from_secs(1), async {
                while executor.attempts.load(std::sync::atomic::Ordering::Acquire) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("invalid protocol response must be inspected");
            assert!(spawner.has_in_flight_cancellation_owners().await);
            assert!(matches!(
                spawner
                    .get_agent_state_any(&agent_id)
                    .await
                    .map(|state| state.status),
                Some(AgentStatus::Waiting { .. })
            ));
        }
    }

    #[tokio::test]
    async fn exact_runtime_fanout_cancel_keeps_child_and_group_runtime_origin() {
        let spawner = DynamicAgentSpawner::new(mock_router()).with_executor(Arc::new(
            ImmediateStatusExecutor {
                status: SPAWN_STATUS_WAITING,
                finish_reason: "waiting",
                output: Some("executor_offline"),
                error: None,
            },
        ));
        let mut input = make_sync_input();
        input.fanout_group_id = Some("runtime-parent-cancel".to_string());
        input.fanout_group_title = Some("Runtime parent cancellation".to_string());
        input.fanout_target_count = Some(1);
        input.fanout_slot_index = Some(0);

        let output = spawner.spawn(input, &make_bg_context()).await.unwrap();
        let agent_id = match output {
            SpawnAgentOutput::Waiting { agent_id, .. } => agent_id,
            other => panic!("expected archived waiting output, got {other:?}"),
        };

        let cancellation = spawner
            .cancel_fanout_group_for_runtime(
                "runtime-parent-cancel",
                "runtime cancelled exact fanout group",
            )
            .await
            .expect("fanout remains queryable");
        assert_eq!(cancellation.stopped_agent_ids, vec![agent_id.clone()]);
        assert!(matches!(
            spawner.get_agent_state_any(&agent_id).await.unwrap().status,
            AgentStatus::Cancelled { by_user: false, .. }
        ));
        let group = spawner
            .fanout_group("runtime-parent-cancel")
            .await
            .expect("fanout remains queryable");
        assert_eq!(
            group.slots[0].status,
            AgentFanoutSlotStatus::CancelledByRuntime
        );
        assert_eq!(group.summary().cancelled_by_runtime, 1);
        assert_eq!(group.summary().cancelled_by_user, 0);
    }

    #[tokio::test]
    async fn exact_unverified_cancel_interrupts_child_without_guessing_origin() {
        let spawner = DynamicAgentSpawner::new(mock_router()).with_executor(Arc::new(
            ImmediateStatusExecutor {
                status: SPAWN_STATUS_WAITING,
                finish_reason: "waiting",
                output: Some("executor_offline"),
                error: None,
            },
        ));
        let output = spawner
            .spawn(make_sync_input(), &make_bg_context())
            .await
            .unwrap();
        let agent_id = match output {
            SpawnAgentOutput::Waiting { agent_id, .. } => agent_id,
            other => panic!("expected archived waiting output, got {other:?}"),
        };

        assert_eq!(
            spawner
                .cancel_agent_with_origin(
                    &agent_id,
                    "unverified exact child cancellation",
                    CancellationOrigin::Unverified,
                )
                .await,
            CancellationTransferOutcome::SeizedPending
        );
        assert!(matches!(
            spawner.get_agent_state_any(&agent_id).await.unwrap().status,
            AgentStatus::Interrupted { ref finish_reason, .. }
                if finish_reason == CANCELLATION_ORIGIN_UNVERIFIED
        ));
    }

    #[tokio::test]
    async fn test_inherited_skills_passed_to_run_config() {
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>);
        let context = SpawnContext {
            parent_run_id: "parent-123".to_string(),
            parent_agent_id: "parent".to_string(),
            resolved_model_name: None,
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
            workspace_mutation: Default::default(),
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
            resolved_model_name: None,
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
            workspace_mutation: Default::default(),
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
                cancellation_origin: CancellationOrigin::Unverified,
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
            resolved_model_name: None,
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
            workspace_mutation: Default::default(),
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
            cancellation_binding_id: None,
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

    #[tokio::test]
    async fn terminal_cleanup_releases_worktree_before_stalled_mailbox_transport() {
        let send_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let unregister_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let transport = Arc::new(PendingTerminalTransport {
            send_started: Arc::clone(&send_started),
            unregister_started: Arc::clone(&unregister_started),
        });
        let tracker = Arc::new(DelegationTracker::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, tracker));
        let parent_addr = astra_messaging::AgentAddress::new("stable-root", "root-agent");
        router
            .record_parent_delivery_alias("root", &parent_addr)
            .await;
        router
            .record_sub_run(astra_messaging::SubRunInfo {
                run_id: "run-99".to_string(),
                parent_run_id: "root".to_string(),
                delegation_id: "root".to_string(),
                agent_id: "agent-99".to_string(),
                depth: 1,
            })
            .await;
        let spawner = DynamicAgentSpawner::new(router);

        let temp = tempfile::TempDir::new().expect("temp directory");
        let worktree = temp.path().join("isolated-child");
        std::fs::create_dir_all(&worktree).expect("isolated worktree");
        let mut state = completed_test_state(99);
        state.status = AgentStatus::Running {
            activity: "finishing".to_string(),
        };
        state.ended_at = None;
        state.messaging_address = Some(astra_messaging::AgentAddress::new(
            state.run_id.clone(),
            state.agent_id.clone(),
        ));
        state.worktree_path = Some(worktree.clone());
        let agent_id = state.agent_id.clone();
        spawner
            .active_agents
            .write()
            .await
            .insert(agent_id.clone(), state);

        assert!(
            spawner
                .finalize_background_agent(
                    &agent_id,
                    AgentStatus::Completed {
                        result: "done".to_string(),
                        finish_reason: Some("normal".to_string()),
                    },
                    "completed",
                    Some("normal"),
                    None,
                    Some("done"),
                    None,
                )
                .await
        );

        tokio::time::timeout(Duration::from_millis(500), async {
            while worktree.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("local worktree cleanup must not wait for transport I/O");
        tokio::time::timeout(Duration::from_millis(500), async {
            while !send_started.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("terminal delivery should reach the stalled transport");
        tokio::time::timeout(Duration::from_secs(2), async {
            while !unregister_started.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("unregister should be attempted after bounded delivery");
        assert!(!worktree.exists());
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

    #[tokio::test]
    async fn completed_agent_archive_never_evicts_nonterminal_cancellation_ownership() {
        let spawner = DynamicAgentSpawner::new(mock_router());
        for index in 0..256 {
            let mut pending = completed_test_state(index);
            pending.status = AgentStatus::Waiting {
                reason: "durable cancellation reconciliation pending".to_string(),
            };
            pending.ended_at = None;
            spawner.archive_state(pending).await;
        }

        spawner.archive_state(completed_test_state(256)).await;
        let completed = spawner.completed_agents.read().await;
        assert_eq!(completed.len(), 257);
        assert!(
            completed
                .iter()
                .take(256)
                .all(|state| matches!(state.status, AgentStatus::Waiting { .. })),
            "history pressure must not evict live cancellation reconciliation ownership"
        );
        assert_eq!(completed.front().unwrap().agent_id, "agent-0");
        assert_eq!(completed.back().unwrap().agent_id, "agent-256");
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
        assert!(
            spawner
                .cancel_agent_for_user(&agent_id, "user-requested")
                .await
                .owns_local_stop()
        );
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
            .cancel_agent_for_user(&agent_id, "user-requested via Ctrl+G x")
            .await;
        assert_eq!(cancelled, CancellationTransferOutcome::AlreadyTerminal);

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
        let result = spawner
            .cancel_agent_for_user("nonexistent@xyz", "test")
            .await;
        assert_eq!(result, CancellationTransferOutcome::NotFound);
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
            .cancel_agent_for_user(&agent_id, "user-requested via Ctrl+G x")
            .await;
        let second = spawner
            .cancel_agent_for_user(&agent_id, "user-requested via Ctrl+G x")
            .await;
        assert_eq!(first, CancellationTransferOutcome::SeizedPending);
        assert!(
            matches!(
                second,
                CancellationTransferOutcome::AlreadyPending
                    | CancellationTransferOutcome::AlreadyTerminal
            ),
            "second cancel must reuse the existing owner or terminal: {second:?}"
        );

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
            .cancel_agent_for_user(&agent_id, "user-requested via Ctrl+G x")
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
            .cancel_agent_for_user(&ids[0], "user-requested via Ctrl+G x")
            .await;
        assert_eq!(cancelled, CancellationTransferOutcome::SeizedPending);

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

        assert!(
            spawner
                .cancel_agent_for_user(&agent_id, "test cleanup")
                .await
                .owns_local_stop()
        );
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
    async fn runtime_cancel_agent_marks_only_target_with_runtime_origin() {
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
                .cancel_agent_for_runtime(&agent_id, "turn budget exhausted")
                .await
                .owns_local_stop(),
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
        assert!(
            matches!(
                &archived.status,
                AgentStatus::Cancelled { by_user: false, .. }
            ),
            "runtime-owned cancellation must not impersonate user control: {:?}",
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
    async fn forced_cancellation_marks_default_metrics_as_unknown_in_journal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let factory = BlockingExecutorFactory::new();
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_session("cancel-metrics".to_string())
            .with_executor(factory as Arc<dyn SpawnAgentExecutor>);

        let launched = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .unwrap();
        let agent_id = match launched {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected Launched, got {other:?}"),
        };
        assert!(
            spawner
                .cancel_agent_for_user(&agent_id, "user cancelled")
                .await
                .owns_local_stop()
        );

        let terminated = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let events =
                    astra_services::session_journal::read_journal("cancel-metrics").unwrap();
                if let Some(terminated) = events.into_iter().find(|event| {
                    event.event_type
                        == astra_services::session_journal::JournalEventType::AgentTerminated
                }) {
                    break terminated;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("best-effort cancellation evidence must settle within its local bound");
        assert_eq!(
            terminated
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("metrics_completeness"))
                .and_then(serde_json::Value::as_str),
            Some("unknown_after_forced_cancellation"),
            "default counters must never be interpreted as measured cancellation work"
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
                .cancel_descendants_of_parent_run_for_user("root")
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
                    by_user: true,
                    ref reason,
                } if reason == DescendantCancellationReason::ancestor_cancelled(
                    CancellationOrigin::User,
                ).as_str()
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
    async fn shutdown_cancels_spawn_stalled_before_task_registration() {
        let register_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(PendingRegisterTransport {
                register_started: Arc::clone(&register_started),
            }),
            Arc::new(DelegationTracker::new()),
        ));
        let spawner = Arc::new(
            DynamicAgentSpawner::new(router)
                .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>),
        );
        let spawn_task = {
            let spawner = Arc::clone(&spawner);
            tokio::spawn(async move { spawner.spawn(make_bg_input(), &make_bg_context()).await })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while !register_started.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("spawn must reach pending mailbox registration");
        assert_eq!(spawner.list_all_agents().await.len(), 1);
        assert_eq!(spawner.background_task_count(), 0);

        tokio::time::timeout(
            Duration::from_secs(1),
            spawner
                .shutdown_and_wait_with_reason(Duration::from_millis(20), "test process shutdown"),
        )
        .await
        .expect("shutdown cannot wait on asynchronous spawn preparation");
        let result = tokio::time::timeout(Duration::from_secs(1), spawn_task)
            .await
            .expect("pre-registration spawn caller must be released")
            .expect("spawn host must not panic");
        assert!(matches!(result, Err(SpawnError::LifecycleShuttingDown)));
        assert!(spawner.list_all_agents().await.is_empty());
        assert_eq!(spawner.background_task_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_close_linearizes_before_the_first_spawn_side_effect() {
        let register_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let router = Arc::new(AgentMailboxRouter::new(
            Arc::new(PendingRegisterTransport {
                register_started: Arc::clone(&register_started),
            }),
            Arc::new(DelegationTracker::new()),
        ));
        let spawner = Arc::new(
            DynamicAgentSpawner::new(router)
                .with_executor(Arc::new(ImmediateSuccessExecutor) as Arc<dyn SpawnAgentExecutor>),
        );
        let reservation_entered = Arc::new(std::sync::Barrier::new(2));
        let release_reservation = Arc::new(std::sync::Barrier::new(2));
        *spawner
            .spawn_before_reservation_barriers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((
            Arc::clone(&reservation_entered),
            Arc::clone(&release_reservation),
        ));

        let spawn_task = {
            let spawner = Arc::clone(&spawner);
            tokio::spawn(async move { spawner.spawn(make_bg_input(), &make_bg_context()).await })
        };
        // The spawn is inside one in-progress poll: the outer biased select has
        // already checked cancellation, but no active/mailbox owner exists yet.
        reservation_entered.wait();

        let shutdown = {
            let spawner = Arc::clone(&spawner);
            tokio::spawn(async move {
                spawner
                    .shutdown_and_wait_with_reason(
                        Duration::from_millis(20),
                        "test reservation race shutdown",
                    )
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while spawner
                .shutdown_active_snapshot_count
                .load(std::sync::atomic::Ordering::SeqCst)
                == 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shutdown must close admission and snapshot active ownership");
        release_reservation.wait();

        tokio::time::timeout(Duration::from_secs(1), shutdown)
            .await
            .expect("shutdown must remain bounded")
            .expect("shutdown task must not panic");
        let result = tokio::time::timeout(Duration::from_secs(1), spawn_task)
            .await
            .expect("racing spawn must return after admission closes")
            .expect("spawn host must not panic");
        assert!(matches!(result, Err(SpawnError::LifecycleShuttingDown)));
        assert!(
            !register_started.load(std::sync::atomic::Ordering::SeqCst),
            "mailbox registration is forbidden after the shutdown linearization point"
        );
        assert!(spawner.list_all_agents().await.is_empty());
        assert_eq!(spawner.background_task_count(), 0);
    }

    #[tokio::test]
    async fn shutdown_cancels_children_concurrently_with_one_shared_deadline() {
        struct PendingDurableCancelExecutor {
            cancel_started: std::sync::atomic::AtomicUsize,
        }

        #[async_trait]
        impl SpawnAgentExecutor for PendingDurableCancelExecutor {
            async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
                std::future::pending::<Result<SpawnRunResult, String>>().await
            }

            async fn cancel_spawned_run(
                &self,
                _run_id: &str,
                _cancellation_binding_id: Option<&str>,
                _user_id: Option<&str>,
                _reason: &str,
                _origin: CancellationOrigin,
            ) -> Result<(), String> {
                self.cancel_started
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::future::pending::<Result<(), String>>().await
            }
        }

        let journal_dir = tempfile::TempDir::new().expect("create journal directory");
        let _journal_guard =
            astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let executor = Arc::new(PendingDurableCancelExecutor {
            cancel_started: std::sync::atomic::AtomicUsize::new(0),
        });
        let spawner = Arc::new(
            DynamicAgentSpawner::new(mock_router())
                .with_session("bounded-shutdown-cancellation".to_string())
                .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>),
        );
        for _ in 0..2 {
            assert!(matches!(
                spawner
                    .spawn(make_bg_input(), &make_bg_context())
                    .await
                    .expect("launch background child"),
                SpawnAgentOutput::Launched { .. }
            ));
        }

        let shutdown_started = tokio::time::Instant::now();
        let shutdown = {
            let spawner = Arc::clone(&spawner);
            tokio::spawn(async move {
                spawner
                    .shutdown_and_wait_with_reason(
                        Duration::from_millis(50),
                        "test bounded process shutdown",
                    )
                    .await
            })
        };
        tokio::time::timeout(Duration::from_millis(250), shutdown)
            .await
            .expect("shutdown must not wait for durable cancellation I/O")
            .expect("shutdown task must not panic");
        assert!(shutdown_started.elapsed() < Duration::from_millis(250));
        assert_eq!(
            executor
                .cancel_started
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an already-closing spawner must hand durable cancellation to recovery instead of starting backend I/O"
        );
        assert!(spawner.list_all_agents().await.is_empty());
        let archived = spawner.completed_agents.read().await;
        assert!(
            archived
                .iter()
                .all(|state| !matches!(state.status, AgentStatus::Cancelled { .. })),
            "a still-pending durable call has no cancellation winner to publish"
        );
        drop(archived);
        assert!(
            spawner.has_in_flight_cancellation_owners().await,
            "shutdown must not report a clean durable drain while cancellation CAS is pending"
        );
        let durable_terminals =
            astra_services::session_journal::read_journal("bounded-shutdown-cancellation")
                .expect("read cancellation journal")
                .into_iter()
                .filter(|event| {
                    event.event_type
                        == astra_services::session_journal::JournalEventType::AgentTerminated
                        && event
                            .metadata
                            .as_ref()
                            .and_then(|metadata| metadata.get("status"))
                            .and_then(serde_json::Value::as_str)
                            == Some("cancelled")
                })
                .count();
        assert_eq!(
            durable_terminals, 0,
            "no cancellation terminal may be journaled before the durable CAS winner is known"
        );
    }

    #[tokio::test]
    async fn cancellation_dual_persistence_failure_never_reports_a_clean_owner() {
        struct RejectingDurableCancel;

        #[async_trait]
        impl SpawnAgentExecutor for RejectingDurableCancel {
            async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
                std::future::pending::<Result<SpawnRunResult, String>>().await
            }

            async fn cancel_spawned_run(
                &self,
                _run_id: &str,
                _cancellation_binding_id: Option<&str>,
                _user_id: Option<&str>,
                _reason: &str,
                _origin: CancellationOrigin,
            ) -> Result<(), String> {
                Err("durable cancellation database unavailable".to_string())
            }
        }

        let temp = tempfile::TempDir::new().expect("create journal parent");
        let invalid_journal_root = temp.path().join("not-a-directory");
        std::fs::write(&invalid_journal_root, b"file blocks journal mkdir")
            .expect("create invalid journal root");
        let _journal_guard =
            astra_services::session_journal::JournalDirGuard::new(&invalid_journal_root);
        let spawner = Arc::new(
            DynamicAgentSpawner::new(mock_router())
                .with_session("dual-persistence-failure".to_string())
                .with_executor(Arc::new(RejectingDurableCancel) as Arc<dyn SpawnAgentExecutor>),
        );
        assert!(matches!(
            spawner
                .spawn(make_bg_input(), &make_bg_context())
                .await
                .expect("launch child"),
            SpawnAgentOutput::Launched { .. }
        ));

        spawner
            .shutdown_and_wait_with_reason(
                Duration::from_millis(100),
                "test dual persistence failure",
            )
            .await;

        assert!(spawner.list_all_agents().await.is_empty());
        assert!(
            spawner.has_in_flight_cancellation_owners().await,
            "journal failure plus durable CAS failure must remain visibly unclean"
        );
        let archived = spawner.completed_agents.read().await;
        assert!(
            archived
                .iter()
                .any(|state| matches!(state.status, AgentStatus::Waiting { .. })),
            "failed durable persistence must remain queryable as pending reconciliation"
        );
        assert!(
            archived
                .iter()
                .all(|state| !matches!(state.status, AgentStatus::Cancelled { .. })),
            "dual persistence failure cannot manufacture a cancellation terminal"
        );
    }

    #[tokio::test]
    async fn failed_cancellation_is_retried_without_a_restart_or_new_turn() {
        struct FailOnceDurableCancel {
            attempts: std::sync::atomic::AtomicUsize,
        }

        #[async_trait]
        impl SpawnAgentExecutor for FailOnceDurableCancel {
            async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
                std::future::pending::<Result<SpawnRunResult, String>>().await
            }

            async fn cancel_spawned_run(
                &self,
                _run_id: &str,
                _cancellation_binding_id: Option<&str>,
                _user_id: Option<&str>,
                _reason: &str,
                _origin: CancellationOrigin,
            ) -> Result<(), String> {
                if self
                    .attempts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    == 0
                {
                    Err("transient durable cancellation failure".to_string())
                } else {
                    Ok(())
                }
            }
        }

        let executor = Arc::new(FailOnceDurableCancel {
            attempts: std::sync::atomic::AtomicUsize::new(0),
        });
        let spawner = Arc::new(
            DynamicAgentSpawner::new(mock_router())
                .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>),
        );
        let agent_id = match spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .expect("launch child")
        {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched child, got {other:?}"),
        };

        assert!(
            spawner
                .cancel_agent_for_user(&agent_id, "retry cancellation")
                .await
                .owns_local_stop(),
            "local cancellation success is independent of durable retry latency"
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while spawner.has_in_flight_cancellation_owners().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the supervised retry must converge in the same process");
        assert!(executor.attempts.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn user_stop_terminalizes_declared_slots_before_late_spawn_reservation() {
        struct CountProviderEntries {
            entries: std::sync::atomic::AtomicUsize,
        }

        #[async_trait]
        impl SpawnAgentExecutor for CountProviderEntries {
            async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
                self.entries
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::future::pending::<Result<SpawnRunResult, String>>().await
            }
        }

        let executor = Arc::new(CountProviderEntries {
            entries: std::sync::atomic::AtomicUsize::new(0),
        });
        let spawner = Arc::new(
            DynamicAgentSpawner::new(mock_router())
                .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>),
        );
        spawner
            .declare_fanout_group(
                "pre-admission-user-stop",
                "Pre-admission user stop",
                1,
                None,
                "root",
            )
            .await
            .expect("declare fixed fanout group");
        let reservation_entered = Arc::new(std::sync::Barrier::new(2));
        let release_reservation = Arc::new(std::sync::Barrier::new(2));
        *spawner
            .spawn_before_reservation_barriers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((
            Arc::clone(&reservation_entered),
            Arc::clone(&release_reservation),
        ));

        let spawn = {
            let spawner = Arc::clone(&spawner);
            tokio::spawn(async move {
                let mut input = make_bg_input();
                input.fanout_group_id = Some("pre-admission-user-stop".to_string());
                input.fanout_target_count = Some(1);
                input.fanout_slot_index = Some(0);
                spawner.spawn(input, &make_bg_context()).await
            })
        };
        reservation_entered.wait();
        let cancellation = spawner
            .cancel_fanout_group_for_user("pre-admission-user-stop", "user stopped declared fanout")
            .await
            .expect("declared fanout remains queryable");
        assert!(cancellation.group.is_terminal());
        assert_eq!(
            cancellation.group.slots[0].status,
            AgentFanoutSlotStatus::CancelledByUser
        );
        assert_eq!(cancellation.group.summary().cancelled_by_user, 1);
        release_reservation.wait();

        let result = tokio::time::timeout(Duration::from_secs(1), spawn)
            .await
            .expect("late spawn must return")
            .expect("spawn task must not panic");
        assert!(matches!(result, Err(SpawnError::Race(_))));
        assert_eq!(
            executor.entries.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a user-settled fixed slot must never enter the provider"
        );
        assert!(spawner.list_all_agents().await.is_empty());
        let group = spawner
            .fanout_group("pre-admission-user-stop")
            .await
            .expect("settled fanout remains queryable");
        assert!(group.is_terminal());
        assert_eq!(group.slots.len(), 1);
        assert_eq!(
            group.slots[0].status,
            AgentFanoutSlotStatus::CancelledByUser
        );
    }

    #[tokio::test]
    async fn fanout_cancel_projects_completed_durable_winner_without_provisional_cancel() {
        struct CompletedWinsCancel;

        #[async_trait]
        impl SpawnAgentExecutor for CompletedWinsCancel {
            async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
                std::future::pending::<Result<SpawnRunResult, String>>().await
            }

            async fn cancel_spawned_run_durably(
                &self,
                _run_id: &str,
                _cancellation_binding_id: Option<&str>,
                _user_id: Option<&str>,
                _reason: &str,
                _origin: CancellationOrigin,
            ) -> Result<SpawnRunCancellationDurability, String> {
                Ok(SpawnRunCancellationDurability::Superseded(
                    AgentStatus::Completed {
                        result: "durable winner".to_string(),
                        finish_reason: Some("normal".to_string()),
                    },
                ))
            }
        }

        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::new(CompletedWinsCancel) as Arc<dyn SpawnAgentExecutor>);
        let mut input = make_bg_input();
        input.fanout_group_id = Some("completed-wins-cancel".to_string());
        input.fanout_target_count = Some(1);
        input.fanout_slot_index = Some(0);
        let agent_id = match spawner
            .spawn(input, &make_bg_context())
            .await
            .expect("launch child")
        {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched child, got {other:?}"),
        };

        let cancellation = spawner
            .cancel_fanout_group_for_user("completed-wins-cancel", "late cancel")
            .await
            .expect("fanout remains queryable");
        assert_eq!(
            cancellation.stopped_agent_ids.len()
                + cancellation.cancellation_pending_agent_ids.len(),
            1,
            "the local ownership transfer must be reported exactly once"
        );
        assert!(
            cancellation.stopped_agent_ids.contains(&agent_id)
                || cancellation
                    .cancellation_pending_agent_ids
                    .contains(&agent_id),
            "the snapshot may report either an already-projected winner or its explicit pending owner"
        );
        assert!(cancellation.not_stopped_agent_ids.is_empty());
        tokio::time::timeout(Duration::from_secs(1), async {
            while spawner.has_in_flight_cancellation_owners().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("durable completed winner must converge in the background");
        let state = spawner
            .get_agent_state_any(&agent_id)
            .await
            .expect("durable winner remains queryable");
        assert!(matches!(
            state.status,
            AgentStatus::Completed { ref result, .. } if result == "durable winner"
        ));
        let group = spawner
            .fanout_group("completed-wins-cancel")
            .await
            .expect("fanout remains queryable");
        let summary = group.summary();
        assert!(group.is_terminal());
        assert_eq!(group.slots[0].status, AgentFanoutSlotStatus::Completed);
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.terminal, 1);
        assert_eq!(summary.cancelled_by_user, 0);
        assert_eq!(summary.cancelled_by_runtime, 0);
        assert!(!spawner.has_in_flight_cancellation_owners().await);
    }

    #[tokio::test]
    async fn fanout_cancel_keeps_opposite_durable_origin_and_separates_request_apis() {
        struct OppositeCancelledWins {
            winner_by_user: bool,
            observed_origins: std::sync::Mutex<Vec<CancellationOrigin>>,
        }

        #[async_trait]
        impl SpawnAgentExecutor for OppositeCancelledWins {
            async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
                std::future::pending::<Result<SpawnRunResult, String>>().await
            }

            async fn cancel_spawned_run_durably(
                &self,
                _run_id: &str,
                _cancellation_binding_id: Option<&str>,
                _user_id: Option<&str>,
                _reason: &str,
                origin: CancellationOrigin,
            ) -> Result<SpawnRunCancellationDurability, String> {
                self.observed_origins.lock().unwrap().push(origin);
                Ok(SpawnRunCancellationDurability::Superseded(
                    AgentStatus::Cancelled {
                        by_user: self.winner_by_user,
                        reason: "opposite durable cancellation winner".to_string(),
                    },
                ))
            }
        }

        for (requested_origin, winner_by_user, expected_slot) in [
            (
                CancellationOrigin::User,
                false,
                AgentFanoutSlotStatus::CancelledByRuntime,
            ),
            (
                CancellationOrigin::Runtime,
                true,
                AgentFanoutSlotStatus::CancelledByUser,
            ),
        ] {
            let group_id = format!("opposite-origin-{}", requested_origin.as_str());
            let executor = Arc::new(OppositeCancelledWins {
                winner_by_user,
                observed_origins: std::sync::Mutex::new(Vec::new()),
            });
            let spawner = DynamicAgentSpawner::new(mock_router())
                .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>);
            let mut input = make_bg_input();
            input.fanout_group_id = Some(group_id.clone());
            input.fanout_target_count = Some(1);
            input.fanout_slot_index = Some(0);
            let agent_id = match spawner
                .spawn(input, &make_bg_context())
                .await
                .expect("launch child")
            {
                SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
                other => panic!("expected launched child, got {other:?}"),
            };

            let cancellation = match requested_origin {
                CancellationOrigin::User => {
                    spawner
                        .cancel_fanout_group_for_user(&group_id, "request user cancellation")
                        .await
                }
                CancellationOrigin::Runtime => {
                    spawner
                        .cancel_fanout_group_for_runtime(&group_id, "request runtime cancellation")
                        .await
                }
                CancellationOrigin::Unverified => unreachable!(),
            }
            .expect("fanout remains queryable");
            assert_eq!(
                cancellation.stopped_agent_ids,
                std::slice::from_ref(&agent_id)
            );
            tokio::time::timeout(Duration::from_secs(1), async {
                while spawner.has_in_flight_cancellation_owners().await {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("opposite durable winner must converge in the background");
            assert_eq!(
                *executor.observed_origins.lock().unwrap(),
                [requested_origin],
                "public user control and internal runtime control must remain typed at the executor boundary"
            );

            let state = spawner
                .get_agent_state_any(&agent_id)
                .await
                .expect("durable winner remains queryable");
            assert!(matches!(
                state.status,
                AgentStatus::Cancelled { by_user, .. } if by_user == winner_by_user
            ));
            let group = spawner
                .fanout_group(&group_id)
                .await
                .expect("fanout remains queryable");
            assert_eq!(group.slots[0].status, expected_slot);
            assert_eq!(
                group.summary().cancelled_by_user,
                usize::from(winner_by_user)
            );
            assert_eq!(
                group.summary().cancelled_by_runtime,
                usize::from(!winner_by_user)
            );
            assert!(!spawner.has_in_flight_cancellation_owners().await);
        }
    }

    #[tokio::test]
    async fn stale_runtime_fanout_cancel_not_owned_by_next_generation_stays_nonterminal() {
        struct NextGenerationOwnsRun {
            observed_binding: std::sync::Mutex<Option<String>>,
        }

        #[async_trait]
        impl SpawnAgentExecutor for NextGenerationOwnsRun {
            async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
                std::future::pending::<Result<SpawnRunResult, String>>().await
            }

            async fn cancel_spawned_run_durably(
                &self,
                _run_id: &str,
                cancellation_binding_id: Option<&str>,
                _user_id: Option<&str>,
                _reason: &str,
                _origin: CancellationOrigin,
            ) -> Result<SpawnRunCancellationDurability, String> {
                *self.observed_binding.lock().unwrap() =
                    cancellation_binding_id.map(ToString::to_string);
                Ok(SpawnRunCancellationDurability::NotOwned(
                    AgentStatus::Running {
                        activity: "active generation N+1".to_string(),
                    },
                ))
            }
        }

        let executor = Arc::new(NextGenerationOwnsRun {
            observed_binding: std::sync::Mutex::new(None),
        });
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>);
        let mut input = make_bg_input();
        input.fanout_group_id = Some("next-generation-owns-run".to_string());
        input.fanout_target_count = Some(1);
        input.fanout_slot_index = Some(0);
        let agent_id = match spawner
            .spawn(input, &make_bg_context())
            .await
            .expect("launch child")
        {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched child, got {other:?}"),
        };
        let expected_binding = spawner
            .get_agent_state_any(&agent_id)
            .await
            .and_then(|state| state.cancellation_binding_id)
            .expect("local spawn must own an immutable cancellation capability");

        let cancellation = spawner
            .cancel_fanout_group_for_runtime(
                "next-generation-owns-run",
                "stale runtime generation cancel",
            )
            .await
            .expect("fanout remains queryable");
        assert!(cancellation.stopped_agent_ids.is_empty());
        assert_eq!(
            cancellation.cancellation_pending_agent_ids,
            std::slice::from_ref(&agent_id)
        );
        assert!(cancellation.not_stopped_agent_ids.is_empty());
        tokio::time::timeout(Duration::from_secs(1), async {
            while spawner.has_in_flight_cancellation_owners().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("authoritative next generation must converge in the background");
        assert_eq!(
            executor.observed_binding.lock().unwrap().as_deref(),
            Some(expected_binding.as_str()),
            "cancellation must present the capability captured by the exact spawn invocation"
        );
        let state = spawner
            .get_agent_state_any(&agent_id)
            .await
            .expect("next-generation projection remains queryable");
        assert!(matches!(
            state.status,
            AgentStatus::Running { ref activity } if activity == "active generation N+1"
        ));
        assert!(state.ended_at.is_none());
        let group = spawner
            .fanout_group("next-generation-owns-run")
            .await
            .expect("fanout remains queryable");
        let summary = group.summary();
        assert!(!group.is_terminal());
        assert_eq!(group.slots[0].status, AgentFanoutSlotStatus::Running);
        assert_eq!(summary.active, 1);
        assert_eq!(summary.terminal, 0);
        assert_eq!(summary.cancelled_by_user, 0);
        assert_eq!(summary.cancelled_by_runtime, 0);
        assert!(
            spawner
                .completion_notifiers
                .read()
                .await
                .contains_key(&agent_id),
            "NotOwned is not completion and must not wake terminal waiters"
        );
        assert!(
            !spawner.has_in_flight_cancellation_owners().await,
            "a confirmed N+1 owner clears this process's stale cancellation owner"
        );
    }

    struct GatedCancellationRetryExecutor {
        attempts: std::sync::atomic::AtomicUsize,
        first: Result<SpawnRunCancellationDurability, String>,
        retry: SpawnRunCancellationDurability,
        retry_started: tokio::sync::Notify,
        release_retry: tokio::sync::Notify,
    }

    #[async_trait]
    impl SpawnAgentExecutor for GatedCancellationRetryExecutor {
        async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            std::future::pending::<Result<SpawnRunResult, String>>().await
        }

        async fn cancel_spawned_run_durably(
            &self,
            _run_id: &str,
            _cancellation_binding_id: Option<&str>,
            _user_id: Option<&str>,
            _reason: &str,
            _origin: CancellationOrigin,
        ) -> Result<SpawnRunCancellationDurability, String> {
            if self
                .attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 0
            {
                return self.first.clone();
            }
            self.retry_started.notify_one();
            self.release_retry.notified().await;
            Ok(self.retry.clone())
        }
    }

    async fn spawn_single_slot_fanout(spawner: &DynamicAgentSpawner, group_id: &str) -> String {
        let mut input = make_bg_input();
        input.fanout_group_id = Some(group_id.to_string());
        input.fanout_target_count = Some(1);
        input.fanout_slot_index = Some(0);
        match spawner
            .spawn(input, &make_bg_context())
            .await
            .expect("launch fanout child")
        {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched fanout child, got {other:?}"),
        }
    }

    async fn assert_pending_cancellation_projection(
        spawner: &DynamicAgentSpawner,
        group_id: &str,
        agent_id: &str,
    ) {
        let state = spawner
            .get_agent_state_any(agent_id)
            .await
            .expect("pending child remains queryable");
        assert!(matches!(state.status, AgentStatus::Waiting { .. }));
        assert!(state.ended_at.is_none());
        let group = spawner
            .fanout_group(group_id)
            .await
            .expect("pending fanout remains queryable");
        let summary = group.summary();
        assert!(!group.is_terminal());
        assert_eq!(
            group.slots[0].status,
            AgentFanoutSlotStatus::WaitingForInput
        );
        assert_eq!(summary.active, 1);
        assert_eq!(summary.terminal, 0);
        assert_eq!(summary.cancelled_by_user, 0);
        assert_eq!(summary.cancelled_by_runtime, 0);
        assert!(
            spawner
                .completion_notifiers
                .read()
                .await
                .contains_key(agent_id),
            "pending durable reconciliation must not notify completion"
        );
        assert!(spawner.has_in_flight_cancellation_owners().await);
    }

    #[tokio::test]
    async fn recovery_recorded_stays_pending_until_retry_terminal_then_notifies() {
        let journal_dir = tempfile::TempDir::new().expect("create journal directory");
        let _journal_guard =
            astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let router = mock_router();
        let mut parent_mailbox = router
            .register(AgentAddress::new("root", "root"), None)
            .await
            .expect("register parent mailbox");
        let executor = Arc::new(GatedCancellationRetryExecutor {
            attempts: std::sync::atomic::AtomicUsize::new(0),
            first: Ok(SpawnRunCancellationDurability::RecoveryRecorded),
            retry: SpawnRunCancellationDurability::Terminal,
            retry_started: tokio::sync::Notify::new(),
            release_retry: tokio::sync::Notify::new(),
        });
        let spawner = DynamicAgentSpawner::new(router)
            .with_session("recovery-then-terminal-session".to_string())
            .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>);
        let group_id = "recovery-then-terminal";
        let agent_id = spawn_single_slot_fanout(&spawner, group_id).await;

        let cancellation = spawner
            .cancel_fanout_group_for_user(group_id, "user stopped pending fanout")
            .await
            .expect("fanout remains queryable");
        assert!(cancellation.stopped_agent_ids.is_empty());
        assert_eq!(
            cancellation.cancellation_pending_agent_ids,
            std::slice::from_ref(&agent_id)
        );
        assert!(cancellation.not_stopped_agent_ids.is_empty());
        assert_pending_cancellation_projection(&spawner, group_id, &agent_id).await;

        tokio::time::timeout(Duration::from_secs(1), executor.retry_started.notified())
            .await
            .expect("retry supervisor must revisit durable recovery intent");
        let completion = spawner
            .completion_notifiers
            .read()
            .await
            .get(&agent_id)
            .cloned()
            .expect("pending child owns a completion notifier");
        let notified = completion.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        executor.release_retry.notify_one();
        tokio::time::timeout(Duration::from_secs(1), notified)
            .await
            .expect("durable Terminal must notify completion");

        let state = spawner
            .get_agent_state_any(&agent_id)
            .await
            .expect("cancelled child remains queryable");
        assert!(matches!(
            state.status,
            AgentStatus::Cancelled { by_user: true, .. }
        ));
        let group = spawner
            .fanout_group(group_id)
            .await
            .expect("terminal fanout remains queryable");
        assert!(group.is_terminal());
        assert_eq!(
            group.slots[0].status,
            AgentFanoutSlotStatus::CancelledByUser
        );
        assert_eq!(group.summary().cancelled_by_user, 1);
        assert!(!spawner.has_in_flight_cancellation_owners().await);
        let parent_result = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(message) = parent_mailbox.try_recv() {
                    break message;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("durable Terminal must reach the parent mailbox");
        assert!(matches!(
            &parent_result.payload,
            MessagePayload::Signal(astra_messaging::AgentSignal::Failed { error })
                if error.contains("cancelled: user stopped pending fanout")
        ));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            parent_mailbox.try_recv().is_none(),
            "one durable terminal winner must produce one parent result"
        );
        let terminal_events =
            astra_services::session_journal::read_journal("recovery-then-terminal-session")
                .expect("read terminal journal")
                .into_iter()
                .filter(|event| {
                    event.event_type
                        == astra_services::session_journal::JournalEventType::AgentTerminated
                })
                .count();
        assert_eq!(terminal_events, 1, "terminal journal must be exact-once");
    }

    #[tokio::test]
    async fn durable_error_stays_pending_until_retry_projects_completed_winner() {
        let journal_dir = tempfile::TempDir::new().expect("create journal directory");
        let _journal_guard =
            astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let router = mock_router();
        let mut parent_mailbox = router
            .register(AgentAddress::new("root", "root"), None)
            .await
            .expect("register parent mailbox");
        let executor = Arc::new(GatedCancellationRetryExecutor {
            attempts: std::sync::atomic::AtomicUsize::new(0),
            first: Err("durable control unavailable".to_string()),
            retry: SpawnRunCancellationDurability::Superseded(AgentStatus::Completed {
                result: "durable completion after retry".to_string(),
                finish_reason: Some("normal".to_string()),
            }),
            retry_started: tokio::sync::Notify::new(),
            release_retry: tokio::sync::Notify::new(),
        });
        let spawner = DynamicAgentSpawner::new(router)
            .with_session("error-then-completed-session".to_string())
            .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>);
        let group_id = "error-then-completed";
        let agent_id = spawn_single_slot_fanout(&spawner, group_id).await;

        let cancellation = spawner
            .cancel_fanout_group_for_runtime(group_id, "runtime deadline")
            .await
            .expect("fanout remains queryable");
        assert!(cancellation.stopped_agent_ids.is_empty());
        assert_eq!(
            cancellation.cancellation_pending_agent_ids,
            std::slice::from_ref(&agent_id)
        );
        assert!(cancellation.not_stopped_agent_ids.is_empty());
        assert_pending_cancellation_projection(&spawner, group_id, &agent_id).await;

        tokio::time::timeout(Duration::from_secs(1), executor.retry_started.notified())
            .await
            .expect("retry supervisor must revisit transient durable error");
        assert_pending_cancellation_projection(&spawner, group_id, &agent_id).await;
        let completion = spawner
            .completion_notifiers
            .read()
            .await
            .get(&agent_id)
            .cloned()
            .expect("pending child owns a completion notifier");
        let notified = completion.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        executor.release_retry.notify_one();
        tokio::time::timeout(Duration::from_secs(1), notified)
            .await
            .expect("completed durable winner must notify completion");

        let state = spawner
            .get_agent_state_any(&agent_id)
            .await
            .expect("completed child remains queryable");
        assert!(matches!(
            state.status,
            AgentStatus::Completed { ref result, .. }
                if result == "durable completion after retry"
        ));
        let group = spawner
            .fanout_group(group_id)
            .await
            .expect("terminal fanout remains queryable");
        assert!(group.is_terminal());
        assert_eq!(group.slots[0].status, AgentFanoutSlotStatus::Completed);
        assert_eq!(group.summary().completed, 1);
        assert_eq!(group.summary().cancelled_by_user, 0);
        assert_eq!(group.summary().cancelled_by_runtime, 0);
        assert!(!spawner.has_in_flight_cancellation_owners().await);
        let parent_result = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(message) = parent_mailbox.try_recv() {
                    break message;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("completed durable winner must reach the parent mailbox");
        assert!(matches!(
            &parent_result.payload,
            MessagePayload::Signal(astra_messaging::AgentSignal::Completed { output })
                if output == "durable completion after retry"
        ));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            parent_mailbox.try_recv().is_none(),
            "one completed durable winner must produce one parent result"
        );
        let terminal_events =
            astra_services::session_journal::read_journal("error-then-completed-session")
                .expect("read terminal journal")
                .into_iter()
                .filter(|event| {
                    event.event_type
                        == astra_services::session_journal::JournalEventType::AgentTerminated
                })
                .count();
        assert_eq!(terminal_events, 1, "terminal journal must be exact-once");
    }

    #[tokio::test]
    async fn durable_scheduler_archives_local_owner_before_first_attempt_and_projects_retry_winner()
    {
        struct RetryWinsBeforeArchive {
            attempts: std::sync::atomic::AtomicUsize,
            first_started: tokio::sync::Notify,
            release_first: tokio::sync::Notify,
            retry_returned_winner: tokio::sync::Notify,
        }

        #[async_trait]
        impl SpawnAgentExecutor for RetryWinsBeforeArchive {
            async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
                std::future::pending::<Result<SpawnRunResult, String>>().await
            }

            async fn cancel_spawned_run_durably(
                &self,
                _run_id: &str,
                _cancellation_binding_id: Option<&str>,
                _user_id: Option<&str>,
                _reason: &str,
                _origin: CancellationOrigin,
            ) -> Result<SpawnRunCancellationDurability, String> {
                let attempt = self
                    .attempts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if attempt == 0 {
                    self.first_started.notify_one();
                    self.release_first.notified().await;
                    Ok(SpawnRunCancellationDurability::RecoveryRecorded)
                } else {
                    self.retry_returned_winner.notify_one();
                    Ok(SpawnRunCancellationDurability::Superseded(
                        AgentStatus::Completed {
                            result: "durable winner".to_string(),
                            finish_reason: Some("normal".to_string()),
                        },
                    ))
                }
            }
        }

        let executor = Arc::new(RetryWinsBeforeArchive {
            attempts: std::sync::atomic::AtomicUsize::new(0),
            first_started: tokio::sync::Notify::new(),
            release_first: tokio::sync::Notify::new(),
            retry_returned_winner: tokio::sync::Notify::new(),
        });
        let spawner = Arc::new(
            DynamicAgentSpawner::new(mock_router())
                .with_executor(Arc::clone(&executor) as Arc<dyn SpawnAgentExecutor>),
        );
        let agent_id = match spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .expect("launch child")
        {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched child, got {other:?}"),
        };

        let cancel = {
            let spawner = Arc::clone(&spawner);
            let agent_id = agent_id.clone();
            tokio::spawn(async move {
                spawner
                    .cancel_agent_for_user(&agent_id, "late cancel")
                    .await
            })
        };
        executor.first_started.notified().await;
        assert!(
            spawner.has_in_flight_cancellation_owners().await,
            "the exact durable owner must remain visible while its first attempt is pending"
        );
        assert!(matches!(
            spawner
                .get_agent_state_any(&agent_id)
                .await
                .map(|state| state.status),
            Some(AgentStatus::Waiting { .. })
        ));

        executor.release_first.notify_one();
        let _ = cancel.await.expect("cancel task");
        executor.retry_returned_winner.notified().await;
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let winner_visible =
                    spawner
                        .get_agent_state_any(&agent_id)
                        .await
                        .is_some_and(|state| {
                            matches!(
                                state.status,
                                AgentStatus::Completed { ref result, .. }
                                    if result == "durable winner"
                            )
                        });
                if winner_visible && !spawner.has_in_flight_cancellation_owners().await {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("retry must project the durable winner after the archive handoff");
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
        assert!(matches!(archived.status, AgentStatus::Waiting { .. }));
        assert!(
            spawner.has_in_flight_cancellation_owners().await,
            "shutdown must retain an explicit pending owner until crash recovery converges"
        );
        let terminal = tokio::time::timeout(Duration::from_secs(1), spawn_task)
            .await
            .expect("foreground caller must be released")
            .expect("foreground spawn host must not panic")
            .expect("foreground spawn should return a terminal payload");
        assert!(matches!(terminal, SpawnAgentOutput::Failed { .. }));
    }

    #[tokio::test]
    async fn shutdown_cancels_local_child_without_taking_remote_observation_ownership() {
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
        let mut root = durable_run("remote-root", 0, astra_core::STATUS_RUNNING);
        root.events.push(json!({
            "type": "agent_spawned",
            "run_id": "remote-child-run",
            "agent_id": "remote-child",
            "agent_type": "review",
            "description": "remote observation"
        }));
        let mut remote = durable_run("remote-child-run", 1, astra_core::STATUS_RUNNING);
        remote.agent_id = Some("remote-child".into());
        remote.owner_pod_id = Some("other-pod".into());
        assert_eq!(spawner.restore_durable_agent_runs(&[root, remote]).await, 1);
        let remote_before = spawner
            .get_agent_state_any("remote-child")
            .await
            .expect("remote observation must remain queryable");

        let launched = spawner
            .spawn(make_bg_input(), &make_bg_context())
            .await
            .expect("launch process-local child");
        let local_agent_id = match launched {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected local background launch, got {other:?}"),
        };
        assert_eq!(
            spawner.list_all_agents().await.len(),
            1,
            "remote durable projections are archived observations, not local execution ownership"
        );

        spawner
            .shutdown_and_wait_with_reason(Duration::from_millis(1), "test pod shutdown")
            .await;

        assert!(spawner.list_all_agents().await.is_empty());
        let local = spawner
            .get_agent_state_any(&local_agent_id)
            .await
            .expect("local child must remain queryable during durable handoff");
        assert!(matches!(local.status, AgentStatus::Waiting { .. }));
        assert!(
            spawner.has_in_flight_cancellation_owners().await,
            "shutdown must not report a clean durable drain before recovery"
        );
        let remote_after = spawner
            .get_agent_state_any("remote-child")
            .await
            .expect("remote observation must survive local shutdown");
        assert_eq!(remote_after.status, remote_before.status);
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
            assert!(
                spawner
                    .cancel_agent_for_user(&agent.agent_id, "test cleanup")
                    .await
                    .owns_local_stop()
            );
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
            resolved_model_name: Some(TEST_CHILD_MODEL.to_string()),
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
            workspace_mutation: Default::default(),
            delegation_chain: Vec::new(),
        }
    }

    fn child_with_inherit(required: bool) -> SpawnAgentInput {
        SpawnAgentInput {
            description: "child".into(),
            prompt: "work".into(),
            agent_type: "explore".into(),
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
    async fn required_prefix_without_store_fails_before_child_execution() {
        let exec = Arc::new(CapturingPrefixExecutor::new());
        let spawner = DynamicAgentSpawner::new(mock_router())
            .with_executor(exec.clone() as Arc<dyn SpawnAgentExecutor>);
        let input = child_with_inherit(true);
        let ctx = parent_context("parent-required");

        let error = spawner
            .spawn(input, &ctx)
            .await
            .expect_err("required inheritance cannot silently degrade when the store is absent");

        assert!(matches!(
            error,
            SpawnError::PrefixInheritanceRequired { .. }
        ));
        assert!(
            exec.take_captured().is_none(),
            "the child executor must not run after required inheritance failed"
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
