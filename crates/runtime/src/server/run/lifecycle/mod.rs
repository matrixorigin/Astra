//! Concrete [`RunLifecycleService`] backed by [`ServerAgenticLoopHost`].
//!
//! This module replaces `UnconfiguredRunLifecycleService` with a real implementation
//! that runs multi-turn agentic loops on the server via the shared
//! [`run_agentic_loop_with_host`] cognitive pipeline.
//!
//! Run status, listing, and replay are backed by durable run state. The
//! process-local map only keeps live control handles for in-flight runs.

mod admission;
mod persistence;
mod projection;
pub(crate) mod run_state;

use admission::*;
use projection::*;
use std::any::Any;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use axum::Json;
use axum::http::StatusCode;
use futures_util::FutureExt;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as TokioMutex, OwnedSemaphorePermit, RwLock, broadcast, mpsc, oneshot};

use astra_server_types::ws_progress_callback::ProgressEvent;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::turn::run_control::{RunControlProvider, RunControlStatus, UserIntentProvider};
use astra_core::{
    ErrorResponse, SharedPool, connect_matrixone, error_response, error_response_coded,
};
use astra_services::ModelService;
use astra_services::coordination::{AgentProfile, AgentTier};
use astra_services::runs::{
    AgentBindingRuntimeRequest, CancelRunRecord, CapabilityServerRefs, ChatRequestData,
    ChatRunRecord, ChatStreamRecord, DurableRunRecord, DurableRunStatusKind, ModelSelectionRequest,
    RequestedTurnInteractionMode, ResolvedModelSelection, RunContinuationRecord,
    RunLifecycleService, RunListCursor, RunListRecord, RunMutationDisposition, RunMutationRecord,
    RunProjectionCheckpointRecord, RunProjectionRecord, RunStatusRecord, RunUserIntentData,
    RunUserIntentRecord, RuntimeAuthRequest, RuntimeProfileRequest,
    durable_run_status_blocks_session, durable_run_status_kind,
};
use astra_services::session_audit::{RUNTIME_PROMOTION_EVENT_TYPE, RuntimePromotionEventData};
use astra_services::session_restore::{
    PROMPT_HISTORY_TRANSCRIPT_EXISTS_SQL, PROMPT_HISTORY_TRANSCRIPT_SELECT_SQL,
    SessionRestoreService,
};
use astra_services::skills::SkillService;
use astra_services::{AdmittedModelExecution, EdgeContext};
use astra_services::{
    DatabaseContextManifestStore, DatabaseStateProjectionStore, RetrievalStage, StateItemUpsert,
};
use astra_services::{
    WorkspaceCleanupDebtEntry, WorkspaceRecordEntry as StoredWorkspaceRecordEntry,
    WorkspaceRecordStoreError, WorkspaceStateStore,
};
use astra_tools::task_mgmt::{SessionTask, TaskManager, TaskStore};
use astra_tools::task_mgmt_matrixone::MatrixOneTaskStore;
use sqlx::Row;

use crate::FernetTokenEncryptor;
use crate::MatrixOneSettings;
use crate::observability::ObservabilityHub;
use crate::orchestration::{
    AgentProgressEvent, AgentToolContext, AgentTranscriptLocation, DurableAgentReconciler,
    DynamicAgentSpawner, InheritedPermissions, PermissionMode, PermissionSyncContext,
    ProgressBroadcaster, ProgressEventType, SpawnAgentExecutor, SpawnRunConfig, SpawnRunResult,
    SpawnedAgentState,
};
use crate::server::run::cloud_workspace_provisioning::CloudWorkspaceProvisioner;
use crate::server::run::workspace_provisioning::{
    ServerWorkspaceProvisionError, ServerWorkspaceProvisioner,
};
use crate::server::session_turn::infer_session_turn;

use crate::turn::agentic_loop::host::{
    AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, CancellationState,
    ContextTracePersistenceContext, EvaluationPersistenceContext, MessagingState,
    RequestConstraints, SkillState, StopHookState, run_agentic_loop_with_host,
};
use crate::{
    DatabaseEvaluationService, DatabaseEventService, DatabaseTraceEventWriter,
    EventCreateRequestData, EventService,
};
use astra_pipeline::step_recorder::StepRecorder;
use astra_turn_core::agent_live_event::{
    AgentLiveEvent, AgentLiveEventKind, AgentLiveEventSink, AgentLiveSendError, AgentLiveSignal,
    AgentLiveTermination, SharedAgentLiveEventSink,
};
use astra_turn_core::contracts::{
    TurnCoreEventRecord, TurnCoreEventWriter, TurnCorePersistPlan, TurnDecisionAuditRecord,
    TurnHookDbPersistPlan, TurnHookDbWriter, TurnObserverRequest, TurnObserverWorker,
    TurnSkillSelectionRecord, TurnToolEventPersistPlan, TurnToolEventRecord, TurnToolEventWriter,
};
use astra_turn_core::interruption::{InterruptionKind, ResumeAction, ResumeMode};
use astra_turn_core::trace_event::{TraceContext, TraceEvent, TraceEventWriter};
use astra_turn_types::UserIntentStatus;

use astra_core::{
    STATUS_CANCELLED, STATUS_COMPLETED, STATUS_DELEGATED, STATUS_FAILED, STATUS_PAUSED,
    STATUS_RUNNING, STATUS_WAITING,
};
use astra_runtime_env::{
    CleanupReason as RuntimeCleanupReason, PolicyIntent as RuntimePolicyIntent,
    WorkspaceOwnerScope as RuntimeWorkspaceOwnerScope,
    WorkspacePersistence as RuntimeWorkspacePersistence,
    WorkspaceProvisionError as RuntimeWorkspaceProvisionError,
    WorkspaceProvisionErrorKind as RuntimeWorkspaceProvisionErrorKind,
    WorkspaceProvisionRequest as RuntimeWorkspaceProvisionRequest, WorkspaceProvisioner,
    WorkspaceRecord as RuntimeWorkspaceRecord, WorkspaceSource as RuntimeWorkspaceSource,
    validate_workspace_id,
};

use crate::orchestration::spawner::{
    DescendantCancellationReason, agent_status_to_progress_event, project_subrun_status_to_spawn,
};
use crate::server::agent_binding_skill_runtime;
use crate::server::deployment_tool_policy::{
    apply_deployment_tool_policy, load_deployment_tool_policy,
};
use crate::server::run::engine::{RunEngine, RunStartContext, TerminalTransitionOutcome};
use crate::server::run::handlers as run_handlers;
use crate::server::runtime_mcp;
use crate::server::server_loop_host::{self, ServerAgenticLoopHostBuilder};
use crate::server::tool_transport::{
    ExecutionBindingSnapshot, ExecutorBinding, ExecutorBindingKind, ExecutorStatus,
    ToolExecutionService, ToolTransportKind, WorkspaceAuthority, WorkspaceBinding,
    WorkspaceBindingKind, binding_event_fields,
};
use crate::server::{runtime_tool_executor, server_skill_subrun};

const MAX_USER_INTENT_CHARS: usize = 20_000;
const MAX_DURABLE_RUN_PROJECTION_RECENT_EVENTS: u32 = 500;
const MAX_ACTIVE_RUN_LIVE_EVENTS: usize = MAX_DURABLE_RUN_PROJECTION_RECENT_EVENTS as usize;
const AGENT_PROGRESS_STREAM_DRAIN_GRACE: Duration = Duration::from_millis(25);
const ATTACHED_INTERACTION_DELIVERY_GRACE: Duration = Duration::from_millis(250);
const HOST_INTERACTION_COMMITTED_FIELD: &str = "_astra_host_interaction_committed";

fn attached_stream_event_requires_reliable_delivery(event: &Value) -> bool {
    matches!(
        event.get("type").and_then(Value::as_str),
        Some(
            "approval_required"
                | "approval_batch_required"
                | "tool_request"
                | "ask_user_prompted"
                | "user_prompt_required"
                | "stream_gap"
                | "agent_live_gap"
        )
    )
}

fn stream_delivery_gap_event(run_id: &str, dropped_event_count: u64) -> Value {
    json!({
        "type": "stream_gap",
        "run_id": run_id,
        "dropped_event_count": dropped_event_count,
        "repair": "refresh_run_snapshot",
    })
}

/// Deliver to the currently attached SSE observer.
///
/// Ordinary progress is deliberately lossy: durable projection and the live
/// broadcast lane can reconstruct it without stalling execution. Human-input
/// boundaries are different. Dropping an approval or ask-user event while the
/// observer is still attached leaves the run waiting for an action the user
/// was never shown, so those events wait for channel capacity instead.
async fn send_attached_stream_event(
    sender: &mut Option<mpsc::Sender<Value>>,
    event: Value,
    run_id: &str,
) {
    if attached_stream_event_requires_reliable_delivery(&event) {
        send_reliable_attached_stream_event(sender, event, run_id).await;
        return;
    }
    let Some(attached) = sender.as_ref() else {
        return;
    };
    match attached.try_send(event) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Closed(_)) => {
            tracing::debug!(
                target: "astra_runtime::run_lifecycle",
                run_id,
                "SSE observer disconnected; durable run continues detached"
            );
            *sender = None;
        }
        Err(mpsc::error::TrySendError::Full(_)) => {
            tracing::warn!(
                target: "astra_runtime::run_lifecycle",
                run_id,
                "SSE observer fell behind; replacing the dropped live event with a durable-repair boundary"
            );
            send_reliable_attached_stream_event(
                sender,
                stream_delivery_gap_event(run_id, 1),
                run_id,
            )
            .await;
        }
    }
}

async fn send_reliable_attached_stream_event(
    sender: &mut Option<mpsc::Sender<Value>>,
    event: Value,
    run_id: &str,
) {
    let Some(attached) = sender.as_ref() else {
        return;
    };
    match tokio::time::timeout(ATTACHED_INTERACTION_DELIVERY_GRACE, attached.send(event)).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            tracing::debug!(
                target: "astra_runtime::run_lifecycle",
                run_id,
                "SSE observer disconnected while delivering a reliable boundary; durable replay remains available"
            );
            *sender = None;
        }
        Err(_) => {
            // Keeping a full attached queue alive while dropping a human-input
            // or repair boundary is worse than ending the stale stream. Close
            // only this observer so reconnect can replay durable truth.
            tracing::warn!(
                target: "astra_runtime::run_lifecycle",
                run_id,
                grace_ms = ATTACHED_INTERACTION_DELIVERY_GRACE.as_millis() as u64,
                "SSE observer stayed full at a reliable boundary; closing the stale delivery lane for durable replay"
            );
            *sender = None;
        }
    }
}

/// Normalize edge-ledger approval presentation events into individually
/// addressable durable interaction facts. A batch is one UI event but each
/// response has its own request identity, so storing only the outer batch
/// would make secure callback lookup impossible (and encourages accepting
/// arbitrary request ids).
fn canonical_edge_approval_requests(event: &Value) -> Vec<Value> {
    let event_type = event.get("type").and_then(Value::as_str);
    let requests: Vec<&Map<String, Value>> = match event_type {
        Some("approval_required") => event.as_object().into_iter().collect(),
        Some("approval_batch_required") => event
            .get("requests")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
            .collect(),
        _ => return Vec::new(),
    };
    requests
        .into_iter()
        .filter_map(|request| {
            if request
                .get("delivery")
                .and_then(Value::as_str)
                .is_some_and(|delivery| delivery != "edge_ledger")
            {
                // DurableRunApprovalGate already committed this request
                // atomically with the run wait before emitting its SSE view.
                return None;
            }
            let request_id = request.get("request_id")?.as_str()?.trim();
            let tool = request.get("tool")?.as_str()?.trim();
            let approval_kind = request
                .get("approval_kind")
                .and_then(Value::as_str)
                .unwrap_or("standard");
            if request_id.is_empty() || tool.is_empty() {
                return None;
            }
            let mut data = Map::from_iter([
                (
                    "request_id".to_string(),
                    Value::String(request_id.to_string()),
                ),
                ("tool".to_string(), Value::String(tool.to_string())),
                (
                    "approval_kind".to_string(),
                    Value::String(approval_kind.to_string()),
                ),
                (
                    "delivery".to_string(),
                    Value::String("edge_ledger".to_string()),
                ),
            ]);
            for field in [
                "path",
                "detail",
                "display_label",
                "run_id",
                "session_id",
                "agent_id",
            ] {
                if let Some(value) = request.get(field).or_else(|| event.get(field)).cloned() {
                    data.insert(field.to_string(), value);
                }
            }
            Some(json!({
                "event_type": "approval_required",
                "idempotency_key": format!("edge-approval-required:{request_id}"),
                "data": data,
            }))
        })
        .collect()
}

fn canonical_edge_tool_request(event: &Value) -> Option<Value> {
    if event.get("type").and_then(Value::as_str) != Some("tool_request") {
        return None;
    }
    let request_id = event.get("request_id")?.as_str()?.trim();
    let tool = event.get("tool")?.as_str()?.trim();
    if request_id.is_empty() || tool.is_empty() {
        return None;
    }
    let mut data = event.as_object()?.clone();
    data.remove("type");
    data.remove(HOST_INTERACTION_COMMITTED_FIELD);
    Some(json!({
        "event_type": "tool_request",
        "idempotency_key": format!("edge-tool-request:{request_id}"),
        "data": data,
    }))
}

fn canonical_edge_interaction_events(event: &Value) -> Vec<Value> {
    let mut interactions = canonical_edge_approval_requests(event);
    interactions.extend(canonical_edge_tool_request(event));
    interactions
}

fn incrementally_persisted_edge_interaction_event(event: &Value) -> bool {
    !canonical_edge_interaction_events(event).is_empty()
}

struct DurableHostInteractionSink {
    run_engine: RunEngine,
    user_id: String,
    run_id: String,
    session_id: String,
    agent_id: Option<String>,
    /// Optional live projection. Durable persistence is the interaction
    /// authority; a detached observer can recover the committed event.
    event_tx: Option<mpsc::Sender<Value>>,
}

#[async_trait]
impl server_loop_host::HostInteractionSink for DurableHostInteractionSink {
    async fn commit_and_deliver(&self, mut event: Value) -> Result<(), String> {
        let event_object = event
            .as_object_mut()
            .ok_or_else(|| "interaction event must be an object".to_string())?;
        event_object
            .entry("run_id".to_string())
            .or_insert_with(|| Value::String(self.run_id.clone()));
        event_object
            .entry("session_id".to_string())
            .or_insert_with(|| Value::String(self.session_id.clone()));
        if let Some(agent_id) = &self.agent_id {
            event_object
                .entry("agent_id".to_string())
                .or_insert_with(|| Value::String(agent_id.clone()));
        }

        let durable_events = canonical_edge_interaction_events(&event);
        if durable_events.is_empty() {
            return Err("interaction event has no durable canonical form".to_string());
        }
        self.run_engine
            .append_events_batch(&self.user_id, &self.run_id, &durable_events)
            .await
            .map_err(|error| format!("interaction persistence failed: {error}"))?;

        let event_object = event
            .as_object_mut()
            .ok_or_else(|| "interaction event must remain an object".to_string())?;
        event_object.insert(
            HOST_INTERACTION_COMMITTED_FIELD.to_string(),
            Value::Bool(true),
        );
        if let Some(event_tx) = &self.event_tx
            && event_tx.send(event).await.is_err()
        {
            tracing::debug!(
                run_id = %self.run_id,
                "durable interaction committed after live observer detached"
            );
        }
        Ok(())
    }
}
const ACTIVE_RUN_DURABLE_CONTROL_WATCH_INTERVAL: Duration = Duration::from_secs(2);
const ACTIVE_RUN_DURABLE_CONTROL_POLL_TIMEOUT: Duration = Duration::from_secs(5);

const RUNTIME_CONTEXT_TRACE_AGENT_ID: &str = "astra-server";
fn should_restore_prior_prompt_history(
    request_targets_existing_session: bool,
    session_has_prior_prompt_history: bool,
) -> bool {
    request_targets_existing_session && session_has_prior_prompt_history
}

fn task_board_settlement_payload(
    snapshot: &crate::turn::agentic_loop::host::TaskBoardSnapshot,
) -> Option<Value> {
    snapshot.has_unfinished_tasks().then(|| {
        json!({
            "summary": snapshot.short_summary(),
            "tracked_count": snapshot.tracked_count,
            "pending_count": snapshot.pending_count,
            "in_progress_count": snapshot.in_progress_count,
            "paused_count": snapshot.paused_count,
            "blocked_count": snapshot.blocked_count,
            "terminal_non_success_count": snapshot.terminal_non_success_count,
            "active_tasks": snapshot.active_tasks,
        })
    })
}

/// Wire a freshly-constructed [`runtime_tool_executor::RuntimeToolExecutor`]
/// into the agentic loop state: Arc-wrap it, attach the task-board monitor,
/// and set the tool-executor handle.  This small helper deduplicates the
/// same three-line pattern repeated at every executor construction site.
fn wire_executor_into_state(
    executor: runtime_tool_executor::RuntimeToolExecutor,
    state: &mut crate::turn::agentic_loop::host::AgenticLoopState,
) {
    let executor = std::sync::Arc::new(executor);
    state.hooks.task_board_monitor = Some(executor.task_manager());
    state.runtime_tool_executor = Some(executor);
}

fn wire_reflect_service_into_executor(
    executor: runtime_tool_executor::RuntimeToolExecutor,
    service: &Arc<dyn astra_services::ReflectService>,
) -> runtime_tool_executor::RuntimeToolExecutor {
    executor.with_reflect_service(Arc::clone(service))
}

fn configure_runtime_semantic_read_cache(
    executor: &mut runtime_tool_executor::RuntimeToolExecutor,
    bundle: Option<&runtime_mcp::RuntimeMcpBundle>,
) {
    let Some(bundle) = bundle else {
        return;
    };
    match bundle.configure_semantic_read_cache(executor) {
        Ok(runtime_tool_executor::SemanticReadCacheActivation::Enabled { binding_count }) => {
            tracing::debug!(
                binding_count,
                "activated production semantic read cache capabilities"
            );
        }
        Ok(runtime_tool_executor::SemanticReadCacheActivation::DisabledNoCapabilities) => {
            tracing::debug!(
                "semantic read cache disabled: runtime bundle has no conditional freshness capabilities"
            );
        }
        Err(error) => tracing::error!(
            %error,
            "semantic read cache composition failed; provider reads remain uncached"
        ),
    }
}

async fn wait_for_shared_run_interaction(
    run_engine: &RunEngine,
    user_id: &str,
    run_id: &str,
    request_id: &str,
    resolved_event_type: &str,
    waiting_for: &str,
    timeout: Duration,
) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(Some(event)) = run_engine
            .load_run_interaction_event(user_id, run_id, request_id, resolved_event_type)
            .await
        {
            return Some(event);
        }
        if matches!(
            run_engine
                .run_is_waiting_for(user_id, run_id, waiting_for)
                .await,
            Ok(false)
        ) {
            // Resolution and status release commit atomically, but they may
            // become visible between the first event read and this status
            // read. Re-read the event after observing the released status so
            // a concurrent peer resolution cannot be mistaken for absence.
            return run_engine
                .load_run_interaction_event(user_id, run_id, request_id, resolved_event_type)
                .await
                .ok()
                .flatten();
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(250).min(deadline.saturating_duration_since(now)))
            .await;
    }
}

async fn project_shared_run_interaction_resolution(
    run_engine: &RunEngine,
    runs: &Arc<RwLock<HashMap<String, RunState>>>,
    user_id: &str,
    run_id: &str,
    request_id: &str,
    resolved_event_type: &str,
    stream_event_tx: Option<&mpsc::Sender<Value>>,
) {
    let mut resolution_events = Vec::with_capacity(2);
    for event_type in [resolved_event_type, "run_resumed"] {
        if let Ok(Some(event)) = run_engine
            .load_run_interaction_event(user_id, run_id, request_id, event_type)
            .await
        {
            resolution_events.push(event);
        }
    }
    if resolution_events.is_empty() {
        return;
    }
    let client_events =
        run_handlers::transform_stream_run_events_for_client(run_id, resolution_events.clone());

    if let Some(stream_event_tx) = stream_event_tx {
        for event in &client_events {
            if stream_event_tx.try_send(event.clone()).is_err() {
                break;
            }
        }
    }

    let live_tx = {
        let mut runs = runs.write().await;
        let Some(run) = runs.get_mut(run_id) else {
            return;
        };
        run.status = RunStatus::Running;
        run.waiting_for = None;
        for event in resolution_events {
            let event_type = event.get("event_type").and_then(Value::as_str);
            let already_projected = run.events.iter().any(|existing| {
                existing.get("event_type").and_then(Value::as_str) == event_type
                    && existing.pointer("/data/request_id").and_then(Value::as_str)
                        == Some(request_id)
            });
            if !already_projected {
                run.events.push(event);
            }
        }
        run.live_tx.clone()
    };
    if let Some(live_tx) = live_tx {
        for event in client_events {
            if live_tx.send(event).is_err() {
                break;
            }
        }
    }
}

fn approval_decision_from_shared_event(event: &Value) -> astra_tools::ApprovalDecision {
    let data = event.get("data").unwrap_or(event);
    match data
        .get("decision")
        .and_then(Value::as_str)
        .unwrap_or("deny")
    {
        "allow" | "allow_session" => astra_tools::ApprovalDecision::Approved,
        "timeout" => astra_tools::ApprovalDecision::Timeout,
        _ => astra_tools::ApprovalDecision::Denied {
            reason: data
                .get("reason")
                .and_then(Value::as_str)
                .map(ToString::to_string),
        },
    }
}

fn ask_user_decision_from_shared_event(event: &Value) -> astra_tools::AskUserDecision {
    let data = event.get("data").unwrap_or(event);
    match data
        .get("outcome")
        .and_then(Value::as_str)
        .unwrap_or("error")
    {
        "submitted" => data
            .get("answers")
            .cloned()
            .and_then(|answers| serde_json::from_value(answers).ok())
            .map(astra_tools::AskUserDecision::Submitted)
            .unwrap_or_else(|| {
                astra_tools::AskUserDecision::Error(
                    "durable ask_user response contains invalid answers".to_string(),
                )
            }),
        "cancelled" => astra_tools::AskUserDecision::Cancelled,
        "timed_out" => astra_tools::AskUserDecision::Timeout,
        _ => astra_tools::AskUserDecision::Error(
            data.get("error")
                .and_then(Value::as_str)
                .unwrap_or("durable ask_user interaction failed")
                .to_string(),
        ),
    }
}

/// Approval gate for a durable server-owned run.
///
/// A server-owned run may outlive every currently attached client.  Treating
/// that as an ordinary tool denial loses the user's opportunity to reconnect
/// and decide.  This gate instead projects a run-scoped `tool_approval` wait,
/// records the request in shared run state, and resumes only if the same
/// durable run is still active when a decision arrives.
struct DurableRunApprovalGate {
    user_id: String,
    context: astra_turn_core::ws_approval_gate::ApprovalJournalContext,
    run_engine: RunEngine,
    runs: Arc<RwLock<HashMap<String, RunState>>>,
    /// Optional WebSocket delivery queue.  It is deliberately not the
    /// authority: a detached or backpressured client must not erase the
    /// durable interaction or turn it into an implicit denial.
    approval_request_tx: Option<mpsc::Sender<Value>>,
    /// The active `/chat/stream` fanout when this run was started through
    /// SSE.  The durable record remains authoritative; this only gives the
    /// currently attached client immediate confirmation instead of making it
    /// discover the wait on its next poll/reconnect.
    stream_event_tx: Option<mpsc::Sender<Value>>,
    /// Cancellation is a real execution boundary, not an approval timeout.
    /// The durable interaction remains replayable, but a cancelled run must stop waiting
    /// immediately and must never execute a late approval.
    cancel_token: Option<Arc<CancellationToken>>,
    timeout: Duration,
    #[cfg(test)]
    wait_started_tx: std::sync::Mutex<Option<oneshot::Sender<()>>>,
}

impl DurableRunApprovalGate {
    const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

    fn new(
        user_id: String,
        session_id: String,
        run_id: String,
        turn: Option<u32>,
        run_engine: RunEngine,
        runs: Arc<RwLock<HashMap<String, RunState>>>,
        approval_request_tx: Option<mpsc::Sender<Value>>,
        stream_event_tx: Option<mpsc::Sender<Value>>,
    ) -> Self {
        Self {
            user_id,
            context: astra_turn_core::ws_approval_gate::ApprovalJournalContext::new(
                session_id, run_id, turn,
            ),
            run_engine,
            runs,
            approval_request_tx,
            stream_event_tx,
            cancel_token: None,
            timeout: Self::DEFAULT_TIMEOUT,
            #[cfg(test)]
            wait_started_tx: std::sync::Mutex::new(None),
        }
    }

    fn with_cancel_token(mut self, cancel_token: Arc<CancellationToken>) -> Self {
        self.cancel_token = Some(cancel_token);
        self
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[cfg(test)]
    fn with_wait_started_notifier(self, wait_started_tx: oneshot::Sender<()>) -> Self {
        *self
            .wait_started_tx
            .lock()
            .expect("approval wait notifier lock") = Some(wait_started_tx);
        self
    }

    #[cfg(test)]
    fn notify_wait_started(&self) {
        if let Some(wait_started_tx) = self
            .wait_started_tx
            .lock()
            .expect("approval wait notifier lock")
            .take()
        {
            let _ = wait_started_tx.send(());
        }
    }

    fn required_event(&self, request_id: &str, tool_name: &str) -> Value {
        json!({
            "event_type": "approval_required",
            "data": {
                "request_id": request_id,
                "tool": tool_name,
                "approval_kind": "standard",
                "delivery": "durable",
                "timeout_ms": self.timeout.as_millis() as u64,
            }
        })
    }

    async fn project_transition(
        &self,
        expected_statuses: &[&str],
        status: RunStatus,
        waiting_for: Option<&str>,
        event: Value,
    ) -> Result<bool, String> {
        let updated = self
            .run_engine
            .transition_status_with_event_if_current(
                &self.user_id,
                &self.context.run_id,
                expected_statuses,
                status.as_str(),
                waiting_for,
                None,
                event.clone(),
            )
            .await?;
        if !updated {
            return Ok(false);
        }

        let mut indexed_event = event.clone();
        if let Ok(Some(run)) = self
            .run_engine
            .load_run(&self.user_id, &self.context.run_id)
            .await
            && let Some(object) = indexed_event.as_object_mut()
        {
            object.insert("index".to_string(), json!(run.last_event_idx));
        }
        let client_events = run_handlers::transform_stream_run_events_for_client(
            &self.context.run_id,
            vec![indexed_event],
        );
        let live_tx = {
            let mut runs = self.runs.write().await;
            runs.get_mut(&self.context.run_id).map(|run| {
                run.status = status;
                run.waiting_for = waiting_for.map(ToString::to_string);
                run.events.push(event);
                run.live_tx.clone()
            })
        }
        .flatten();
        // Durable state is already committed. Publish to the reconnectable
        // live lane before waiting on one attached client's bounded queue, so
        // a stalled observer cannot hide an interaction boundary from every
        // other observer.
        if let Some(live_tx) = live_tx {
            for event in &client_events {
                if live_tx.send(event.clone()).is_err() {
                    tracing::debug!(
                        target: "astra_runtime::run_lifecycle",
                        run_id = %self.context.run_id,
                        "approval transition has no live stream subscribers"
                    );
                    break;
                }
            }
        };
        if let Some(stream_event_tx) = &self.stream_event_tx {
            for event in &client_events {
                if stream_event_tx.send(event.clone()).await.is_err() {
                    tracing::debug!(
                        target: "astra_runtime::run_lifecycle",
                        run_id = %self.context.run_id,
                        "approval transition stream is detached; durable replay remains available"
                    );
                    break;
                }
            }
        }
        Ok(true)
    }

    fn denied_due_to_no_longer_active() -> astra_tools::ApprovalDecision {
        astra_tools::ApprovalDecision::Denied {
            reason: Some(
                "approval response arrived after this run stopped waiting; the tool was not executed"
                    .to_string(),
            ),
        }
    }
}

#[async_trait]
impl astra_tools::ToolApprovalGate for DurableRunApprovalGate {
    async fn request_approval(
        &self,
        request_id: &str,
        tool_name: &str,
        args: &Value,
    ) -> astra_tools::ApprovalDecision {
        let wait_started = match self
            .project_transition(
                &[STATUS_RUNNING],
                RunStatus::Waiting,
                Some("tool_approval"),
                self.required_event(request_id, tool_name),
            )
            .await
        {
            Ok(true) => true,
            Ok(false) => return Self::denied_due_to_no_longer_active(),
            Err(error) => {
                tracing::warn!(
                    target: "astra_runtime::run_lifecycle",
                    run_id = %self.context.run_id,
                    request_id,
                    error = %error,
                    "failed to persist Server Only approval wait"
                );
                return astra_tools::ApprovalDecision::Denied {
                    reason: Some(
                        "approval wait could not be recorded durably; tool was not executed".into(),
                    ),
                };
            }
        };
        debug_assert!(wait_started);
        #[cfg(test)]
        self.notify_wait_started();

        if let Some(approval_request_tx) = &self.approval_request_tx {
            let request = json!({
                "request_id": request_id,
                "tool": tool_name,
                "args": args,
                "session_id": &self.context.session_id,
                "run_id": &self.context.run_id,
            });
            match approval_request_tx.try_send(request) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Closed(_)) => tracing::debug!(
                    target: "astra_runtime::run_lifecycle",
                    run_id = %self.context.run_id,
                    request_id,
                    "interactive approval delivery is detached; durable replay remains available"
                ),
                Err(mpsc::error::TrySendError::Full(_)) => tracing::warn!(
                    target: "astra_runtime::run_lifecycle",
                    run_id = %self.context.run_id,
                    request_id,
                    "interactive approval delivery is backpressured; durable replay remains available"
                ),
            }
        }

        let resolved = if let Some(cancel_token) = &self.cancel_token {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    return astra_tools::ApprovalDecision::Denied {
                        reason: Some("run was cancelled before approval resolved; the tool was not executed".to_string()),
                    };
                }
                resolved = wait_for_shared_run_interaction(
                    &self.run_engine,
                    &self.user_id,
                    &self.context.run_id,
                    request_id,
                    astra_services::runs::DurableRunInteractionKind::Approval.resolved_event_type(),
                    astra_services::runs::DurableRunInteractionKind::Approval.waiting_for(),
                    self.timeout,
                ) => resolved,
            }
        } else {
            wait_for_shared_run_interaction(
                &self.run_engine,
                &self.user_id,
                &self.context.run_id,
                request_id,
                astra_services::runs::DurableRunInteractionKind::Approval.resolved_event_type(),
                astra_services::runs::DurableRunInteractionKind::Approval.waiting_for(),
                self.timeout,
            )
            .await
        };
        if let Some(resolved) = resolved {
            project_shared_run_interaction_resolution(
                &self.run_engine,
                &self.runs,
                &self.user_id,
                &self.context.run_id,
                request_id,
                astra_services::runs::DurableRunInteractionKind::Approval.resolved_event_type(),
                self.stream_event_tx.as_ref(),
            )
            .await;
            return approval_decision_from_shared_event(&resolved);
        }
        let timeout_data = json!({
            "request_id": request_id,
            "outcome": "timed_out",
            "decision": "timeout",
            "reason": "approval deadline elapsed",
            "tool": tool_name,
            "approval_kind": "standard",
        });
        match self
            .run_engine
            .resolve_run_interaction(
                &self.user_id,
                &self.context.run_id,
                request_id,
                astra_services::runs::DurableRunInteractionKind::Approval,
                timeout_data,
            )
            .await
        {
            Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Resolved(event))
            | Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Idempotent(event))
            | Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Conflict(event)) => {
                project_shared_run_interaction_resolution(
                    &self.run_engine,
                    &self.runs,
                    &self.user_id,
                    &self.context.run_id,
                    request_id,
                    astra_services::runs::DurableRunInteractionKind::Approval.resolved_event_type(),
                    self.stream_event_tx.as_ref(),
                )
                .await;
                approval_decision_from_shared_event(&event)
            }
            Ok(astra_services::runs::DurableRunInteractionResolveOutcome::MissingRequest)
            | Ok(astra_services::runs::DurableRunInteractionResolveOutcome::NoLongerWaiting) => {
                Self::denied_due_to_no_longer_active()
            }
            Err(error) => {
                tracing::warn!(
                    target: "astra_runtime::run_lifecycle",
                    run_id = %self.context.run_id,
                    request_id,
                    error = %error,
                    "failed to commit durable approval timeout"
                );
                astra_tools::ApprovalDecision::Denied {
                    reason: Some(
                        "approval deadline could not be closed durably; tool was not executed"
                            .into(),
                    ),
                }
            }
        }
    }

    fn requires_approval(&self, tool_name: &str) -> bool {
        astra_tools::APPROVAL_REQUIRED_TOOLS.contains(&tool_name)
    }
}

/// Durable `ask_user` gate for every server-owned run.
///
/// The user prompt is a run interaction, not a property of a currently open
/// WebSocket. Shared durable run state is written before any
/// delivery attempt; WebSocket and SSE queues merely make an attached client
/// responsive sooner.
struct DurableRunUserPromptGate {
    user_id: String,
    context: astra_turn_core::ws_user_prompt_gate::UserPromptJournalContext,
    run_engine: RunEngine,
    runs: Arc<RwLock<HashMap<String, RunState>>>,
    user_prompt_request_tx: Option<mpsc::Sender<Value>>,
    stream_event_tx: Option<mpsc::Sender<Value>>,
    /// See [`DurableRunApprovalGate::cancel_token`]. A cancelled run must not
    /// remain parked in an input wait until its generic timeout.
    cancel_token: Option<Arc<CancellationToken>>,
    timeout: Duration,
}

impl DurableRunUserPromptGate {
    const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

    fn new(
        user_id: String,
        session_id: String,
        run_id: String,
        turn: Option<u32>,
        run_engine: RunEngine,
        runs: Arc<RwLock<HashMap<String, RunState>>>,
        user_prompt_request_tx: Option<mpsc::Sender<Value>>,
        stream_event_tx: Option<mpsc::Sender<Value>>,
    ) -> Self {
        Self {
            user_id,
            context: astra_turn_core::ws_user_prompt_gate::UserPromptJournalContext::new(
                session_id, run_id, turn,
            ),
            run_engine,
            runs,
            user_prompt_request_tx,
            stream_event_tx,
            cancel_token: None,
            timeout: Self::DEFAULT_TIMEOUT,
        }
    }

    fn with_cancel_token(mut self, cancel_token: Arc<CancellationToken>) -> Self {
        self.cancel_token = Some(cancel_token);
        self
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn required_event(&self, request_id: &str, prompt: &astra_tools::AskUserPrompt) -> Value {
        json!({
            "event_type": "ask_user_prompted",
            "data": {
                "request_id": request_id,
                "prompt": prompt,
                "delivery": "durable",
                "timeout_ms": prompt.timeout_ms.unwrap_or(self.timeout.as_millis() as u64),
            }
        })
    }

    async fn project_transition(
        &self,
        expected_statuses: &[&str],
        status: RunStatus,
        waiting_for: Option<&str>,
        event: Value,
    ) -> Result<bool, String> {
        let updated = self
            .run_engine
            .transition_status_with_event_if_current(
                &self.user_id,
                &self.context.run_id,
                expected_statuses,
                status.as_str(),
                waiting_for,
                None,
                event.clone(),
            )
            .await?;
        if !updated {
            return Ok(false);
        }

        let mut indexed_event = event.clone();
        if let Ok(Some(run)) = self
            .run_engine
            .load_run(&self.user_id, &self.context.run_id)
            .await
            && let Some(object) = indexed_event.as_object_mut()
        {
            object.insert("index".to_string(), json!(run.last_event_idx));
        }
        let client_events = run_handlers::transform_stream_run_events_for_client(
            &self.context.run_id,
            vec![indexed_event],
        );
        let live_tx = {
            let mut runs = self.runs.write().await;
            runs.get_mut(&self.context.run_id).map(|run| {
                run.status = status;
                run.waiting_for = waiting_for.map(ToString::to_string);
                run.events.push(event);
                run.live_tx.clone()
            })
        }
        .flatten();
        if let Some(live_tx) = live_tx {
            for event in &client_events {
                if live_tx.send(event.clone()).is_err() {
                    break;
                }
            }
        };
        if let Some(stream_event_tx) = &self.stream_event_tx {
            for event in &client_events {
                if stream_event_tx.send(event.clone()).await.is_err() {
                    tracing::debug!(
                        target: "astra_runtime::run_lifecycle",
                        run_id = %self.context.run_id,
                        "user prompt transition stream is detached; durable replay remains available"
                    );
                    break;
                }
            }
        }
        Ok(true)
    }

    fn no_longer_active() -> astra_tools::AskUserDecision {
        astra_tools::AskUserDecision::Error(
            "user prompt response arrived after this run stopped waiting".to_string(),
        )
    }
}

#[async_trait]
impl astra_tools::AskUserGate for DurableRunUserPromptGate {
    async fn request_questionnaire(
        &self,
        request_id: &str,
        prompt: &astra_tools::AskUserPrompt,
    ) -> astra_tools::AskUserDecision {
        match self
            .project_transition(
                &[STATUS_RUNNING],
                RunStatus::Waiting,
                Some("user_input"),
                self.required_event(request_id, prompt),
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => return Self::no_longer_active(),
            Err(error) => {
                tracing::warn!(
                    target: "astra_runtime::run_lifecycle",
                    run_id = %self.context.run_id,
                    request_id,
                    error = %error,
                    "failed to persist durable ask_user wait"
                );
                return astra_tools::AskUserDecision::Error(
                    "ask_user could not be recorded durably".to_string(),
                );
            }
        }

        if let Some(user_prompt_request_tx) = &self.user_prompt_request_tx {
            let request = json!({
                "request_id": request_id,
                "session_id": &self.context.session_id,
                "run_id": &self.context.run_id,
                "prompt": prompt,
            });
            match user_prompt_request_tx.try_send(request) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Closed(_)) => tracing::debug!(
                    target: "astra_runtime::run_lifecycle",
                    run_id = %self.context.run_id,
                    request_id,
                    "user prompt delivery is detached; durable callback remains available"
                ),
                Err(mpsc::error::TrySendError::Full(_)) => tracing::warn!(
                    target: "astra_runtime::run_lifecycle",
                    run_id = %self.context.run_id,
                    request_id,
                    "user prompt delivery is backpressured; durable callback remains available"
                ),
            }
        }

        let timeout = prompt
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.timeout);
        let resolved = if let Some(cancel_token) = &self.cancel_token {
            tokio::select! {
                _ = cancel_token.cancelled() => return astra_tools::AskUserDecision::Cancelled,
                resolved = wait_for_shared_run_interaction(
                    &self.run_engine,
                    &self.user_id,
                    &self.context.run_id,
                    request_id,
                    astra_services::runs::DurableRunInteractionKind::AskUser.resolved_event_type(),
                    astra_services::runs::DurableRunInteractionKind::AskUser.waiting_for(),
                    timeout,
                ) => resolved,
            }
        } else {
            wait_for_shared_run_interaction(
                &self.run_engine,
                &self.user_id,
                &self.context.run_id,
                request_id,
                astra_services::runs::DurableRunInteractionKind::AskUser.resolved_event_type(),
                astra_services::runs::DurableRunInteractionKind::AskUser.waiting_for(),
                timeout,
            )
            .await
        };
        if let Some(resolved) = resolved {
            project_shared_run_interaction_resolution(
                &self.run_engine,
                &self.runs,
                &self.user_id,
                &self.context.run_id,
                request_id,
                astra_services::runs::DurableRunInteractionKind::AskUser.resolved_event_type(),
                self.stream_event_tx.as_ref(),
            )
            .await;
            return ask_user_decision_from_shared_event(&resolved);
        }
        let timeout_data = json!({
            "request_id": request_id,
            "outcome": "timed_out",
        });
        match self
            .run_engine
            .resolve_run_interaction(
                &self.user_id,
                &self.context.run_id,
                request_id,
                astra_services::runs::DurableRunInteractionKind::AskUser,
                timeout_data,
            )
            .await
        {
            Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Resolved(event))
            | Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Idempotent(event))
            | Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Conflict(event)) => {
                project_shared_run_interaction_resolution(
                    &self.run_engine,
                    &self.runs,
                    &self.user_id,
                    &self.context.run_id,
                    request_id,
                    astra_services::runs::DurableRunInteractionKind::AskUser.resolved_event_type(),
                    self.stream_event_tx.as_ref(),
                )
                .await;
                ask_user_decision_from_shared_event(&event)
            }
            Ok(astra_services::runs::DurableRunInteractionResolveOutcome::MissingRequest)
            | Ok(astra_services::runs::DurableRunInteractionResolveOutcome::NoLongerWaiting) => {
                Self::no_longer_active()
            }
            Err(error) => astra_tools::AskUserDecision::Error(format!(
                "ask_user deadline could not be closed durably: {error}"
            )),
        }
    }
}

use crate::server::run::binding_resolution::{
    RunExecutionBindingSnapshot, agent_working_dir_for_bindings, binding_snapshot_events,
    execution_bindings_from_edge_profile, execution_bindings_from_metadata,
    executor_binding_from_request, request_uses_server_workspace,
    resolve_request_execution_bindings,
    resolve_request_execution_bindings_without_server_workspace, run_start_context_from_request,
};

fn panic_payload_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Cap on concurrent live subagents per server-side run. Mirrors the
/// CLI-side cap in `crate::cli::agent_runtime::resolved_spawn_concurrency_cap`
/// so headless web sessions don't have a different ceiling than the
/// terminal CLI. Override via `ASTRA_MAX_CONCURRENT_AGENTS=N`.
fn resolved_server_spawn_concurrency_cap() -> usize {
    const DEFAULT_CAP: usize = 10;
    match std::env::var("ASTRA_MAX_CONCURRENT_AGENTS") {
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => {
                tracing::warn!(
                    target: "astra::spawner",
                    raw = %raw,
                    default = DEFAULT_CAP,
                    "ASTRA_MAX_CONCURRENT_AGENTS unparseable; using default"
                );
                DEFAULT_CAP
            }
        },
        Err(_) => DEFAULT_CAP,
    }
}

async fn run_agentic_loop_with_host_panic_safe<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<AgenticLoopOutcome, astra_core::ClassifiedError> {
    match AssertUnwindSafe(run_agentic_loop_with_host(host, state))
        .catch_unwind()
        .await
    {
        Ok(outcome) => outcome,
        Err(payload) => {
            let message = format!(
                "agentic loop panicked: {}",
                panic_payload_message(payload.as_ref())
            );
            tracing::error!(
                target: "astra_runtime::run_lifecycle",
                error = %message,
                "agentic loop panic converted to failed run"
            );
            Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::Unknown,
                message,
            ))
        }
    }
}

/// Bind a turn-scoped Server root run to one session-stable mailbox. Child
/// runs can report checkpoints and terminal results while the parent is
/// active, or queue them for the next turn after it settles.
async fn install_server_root_mailbox(
    state: &mut AgenticLoopState,
    router: &Arc<astra_messaging::router::AgentMailboxRouter>,
    session_id: &str,
    run_id: &str,
    agent_id: &str,
) {
    if state.messaging.mailbox.is_some() {
        return;
    }
    let address = astra_messaging::AgentAddress::new(session_id, agent_id);
    match router.register(address.clone(), None).await {
        Ok(mailbox) => {
            router.record_parent_delivery_alias(run_id, &address).await;
            state.messaging.mailbox = Some(mailbox);
        }
        Err(error) => tracing::warn!(
            target: "astra_runtime::messaging",
            session_id,
            run_id,
            agent_id,
            error = %error,
            "server root mailbox registration failed; child delivery will remain observable only through lifecycle events"
        ),
    }
}

/// Unregister the Server root mailbox without losing messages that arrived
/// after the final model boundary. Those messages are acknowledged on the old
/// stream and re-sent through Parent routing, which parks them under the same
/// stable session mailbox for the next turn.
async fn park_server_root_mailbox(state: &mut AgenticLoopState) {
    const MAX_LATE_ROOT_MESSAGES: usize = 256;
    let Some(mut mailbox) = state.messaging.mailbox.take() else {
        return;
    };
    let address = mailbox.address.clone();
    let router = mailbox.router();
    let (late_messages, has_more) = mailbox.drain_bounded(MAX_LATE_ROOT_MESSAGES);
    if let Err(error) = mailbox.acknowledge_received(&late_messages).await {
        // Durable transports will release unacknowledged claims when this
        // stream drops. Re-inserting as well would create two deliveries, so
        // leave recovery to the transport in this branch.
        tracing::warn!(
            target: "astra_runtime::messaging",
            address = %address,
            error = %error,
            "late server root messages could not be acknowledged; transport recovery will retain them"
        );
        let _ = router.unregister(&address).await;
        return;
    }
    if let Err(error) = router.unregister(&address).await {
        tracing::warn!(
            target: "astra_runtime::messaging",
            address = %address,
            error = %error,
            "server root mailbox unregister failed"
        );
    }
    for message in late_messages {
        let mut parked = (*message).clone();
        // The durable queue keys rows by message id. The original delivery is
        // now acknowledged, so parking is a new delivery attempt with the
        // same semantic/correlation payload but a fresh envelope identity.
        parked.id = uuid::Uuid::now_v7().to_string();
        parked.ack_message_id = message.requires_ack.then(|| message.id.clone());
        parked.to = astra_messaging::MessageTarget::Parent;
        if let Err(error) = router.send(parked).await {
            tracing::warn!(
                target: "astra_runtime::messaging",
                message_id = %message.id,
                error = %error,
                "late server root message could not be parked for the next turn"
            );
        }
    }
    if has_more {
        tracing::warn!(
            target: "astra_runtime::messaging",
            address = %address,
            limit = MAX_LATE_ROOT_MESSAGES,
            "server root mailbox exceeded the bounded late-message parking window"
        );
    }
}

// ─── Skill wiring for server paths ──────────────────────────────────────────

type ServerSkillResolverBundle = (
    Option<Arc<crate::skills::UnifiedSkillRegistry>>,
    Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
);

/// Post-loop session-memory cleanup shared by `create_run` and
/// `stream_chat`. Schedules session-end governance (purge working memory +
/// persist episodic overview + Memoria reflect) when the per-session
/// debounce window allows, and clears only run-scoped attribution/extraction
/// state. Session-scoped selections, surfaced identities, and focus survive
/// follow-up runs and are bounded independently by the memory runtime.
///
/// Best-effort: every step logs and continues on failure. Safe to call
/// with an empty `session_id` (no-op).
async fn post_loop_memory_cleanup(
    session_id: &str,
    session_facts: &astra_turn_types::session_facts::SessionFacts,
    extraction_service: Option<&Arc<crate::session_memory::MemoryExtractionService>>,
    final_extract_request: Option<crate::session_memory::ExtractionRequest>,
    metrics_registry: Option<Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>>,
) {
    post_loop_memory_cleanup_with_limits(
        session_id,
        session_facts,
        extraction_service,
        final_extract_request,
        metrics_registry,
        DEFAULT_POST_LOOP_MEMORY_CLEANUP_CONCURRENCY,
        Duration::from_millis(DEFAULT_SESSION_MEMORY_POST_LOOP_DRAIN_TIMEOUT_MS),
    )
    .await;
}

async fn post_loop_memory_cleanup_with_limits(
    session_id: &str,
    session_facts: &astra_turn_types::session_facts::SessionFacts,
    extraction_service: Option<&Arc<crate::session_memory::MemoryExtractionService>>,
    final_extract_request: Option<crate::session_memory::ExtractionRequest>,
    metrics_registry: Option<Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>>,
    async_concurrency_limit: usize,
    drain_timeout: Duration,
) {
    if session_id.is_empty() {
        return;
    }

    let session_id = session_id.to_string();
    let session_facts = session_facts.clone();
    let extraction_service = extraction_service.cloned();

    let Some(permit) = try_acquire_post_loop_memory_cleanup_permit(async_concurrency_limit) else {
        record_post_loop_memory_cleanup_dispatch_metrics(
            metrics_registry.as_ref(),
            "inline",
            "saturated",
        );
        tracing::warn!(
            session_id = %session_id,
            concurrency_limit = async_concurrency_limit,
            "post-loop memory cleanup concurrency full; running inline"
        );
        run_post_loop_memory_cleanup_work(
            session_id,
            session_facts,
            extraction_service,
            final_extract_request,
            metrics_registry,
            drain_timeout,
        )
        .await;
        return;
    };
    record_post_loop_memory_cleanup_dispatch_metrics(
        metrics_registry.as_ref(),
        "async",
        "scheduled",
    );
    tokio::spawn(async move {
        let _permit = permit;
        run_post_loop_memory_cleanup_work(
            session_id,
            session_facts,
            extraction_service,
            final_extract_request,
            metrics_registry,
            drain_timeout,
        )
        .await;
    });
}

async fn run_post_loop_memory_cleanup_work(
    session_id: String,
    session_facts: astra_turn_types::session_facts::SessionFacts,
    extraction_service: Option<Arc<crate::session_memory::MemoryExtractionService>>,
    final_extract_request: Option<crate::session_memory::ExtractionRequest>,
    metrics_registry: Option<Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>>,
    drain_timeout: Duration,
) {
    if let (Some(svc), Some(req)) = (extraction_service.as_ref(), final_extract_request) {
        let _ = svc.maybe_spawn_shutdown_flush(req);
    }
    if let Some(svc) = extraction_service.as_ref() {
        let leftover = if drain_timeout.is_zero() {
            svc.pending_drain()
        } else {
            svc.wait_for_pending(drain_timeout).await
        };
        if leftover > 0 {
            record_session_memory_post_loop_drain_metrics(metrics_registry.as_ref(), "leftover");
            tracing::warn!(
                session_id = %session_id,
                leftover,
                "session-memory extraction still in flight after post-loop drain timeout"
            );
        } else {
            record_session_memory_post_loop_drain_metrics(metrics_registry.as_ref(), "clean");
        }
    } else {
        record_session_memory_post_loop_drain_metrics(metrics_registry.as_ref(), "no_service");
    }
    // ── Governance, debounced ──
    //
    // Session IDs are sticky across many terminal runs (user reopens a
    // session or the TUI issues follow-up turns). Running governance
    // per run would write one episode per turn and hammer reflect.
    // The debouncer allows one governance per session per window.
    let debouncer = crate::turn::session_end_debounce::global();
    if matches!(
        debouncer.should_run(&session_id),
        crate::turn::session_end_debounce::DebounceDecision::Run
    ) {
        let report = if let Some(svc) = extraction_service.as_ref() {
            Some(
                svc.run_session_end_governance(&session_facts, &session_id)
                    .await,
            )
        } else if let Some(memoria_client) =
            crate::turn::cloud::memoria_compact::HttpMemoriaPort::from_env()
        {
            Some(
                crate::turn::cloud::session_end_governance::run_session_end_governance(
                    &session_facts,
                    &session_id,
                    &memoria_client,
                )
                .await,
            )
        } else {
            None
        };

        match report {
            Some(Ok(report)) => {
                if report.episode_chars > 0
                    || report.working_purged > 0
                    || report.working_retained_due_to_episode_failure
                    || report.reflect_candidates > 0
                    || report.scenes_stored > 0
                {
                    tracing::info!(
                        session_id = %session_id,
                        purged = report.working_purged,
                        working_retained = report.working_retained_due_to_episode_failure,
                        episode_chars = report.episode_chars,
                        reflect_candidates = report.reflect_candidates,
                        reflect_synthesized = report.reflect_synthesized,
                        scenes_stored = report.scenes_stored,
                        "session-end governance complete"
                    );
                }
                debouncer.record(&session_id);
            }
            Some(Err(error)) => tracing::warn!(
                session_id = %session_id,
                error = %error,
                "session-end governance failed"
            ),
            None => tracing::debug!(
                session_id = %session_id,
                "session-end governance skipped because no memory provider is configured"
            ),
        }
    } else {
        tracing::debug!(
            session_id = %session_id,
            "session-end governance skipped by debounce"
        );
    }

    // Unattributed recall is not positive evidence. A productive session may
    // have ignored or worked around a bad memory, so session end never marks
    // every surfaced item `useful`. Tool/user outcome paths send feedback only
    // when they can attribute useful/outdated/wrong to a concrete memory id.
    finish_post_loop_memory_run_boundary(&session_id, extraction_service.as_ref());
    record_post_loop_memory_cleanup_worker_metrics(metrics_registry.as_ref(), "completed");
}

fn finish_post_loop_memory_run_boundary(
    session_id: &str,
    extraction_service: Option<&Arc<crate::session_memory::MemoryExtractionService>>,
) {
    // Recall attribution is producer/run-scoped and is settled by the turn
    // that created it. A session-only post-loop hook must not drain another
    // concurrent run's observations. Release only extraction debounce here.
    if let Some(svc) = extraction_service {
        svc.forget_session(session_id);
    }
}

/// Build a user-scoped skill registry + resolver for server-side web runs.
///
/// The visible catalog is assembled by `skills::catalog` and contains exactly:
/// API-server HOME skills (`~/.astra/skills`, `~/.claude/skills`) plus database
/// skills visible to the authenticated user. Request `allow_skills` is a
/// selector/execution filter over that catalog, not a switch that enables the
/// catalog.
fn build_server_skill_resolver(
    skill_service: Option<Arc<dyn SkillService>>,
    user_id: &str,
) -> ServerSkillResolverBundle {
    use crate::turn::skill_tool::SkillResolver as _;

    let Some(registry) = crate::capabilities::build_server_skill_registry(skill_service, user_id)
    else {
        return (None, None);
    };

    let resolver_impl = Arc::new(crate::skills::UnifiedSkillResolver::new(Arc::clone(
        &registry,
    )));
    let skills = resolver_impl.available_skills();
    let resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>> = if skills.is_empty() {
        None
    } else {
        Some(resolver_impl)
    };
    (Some(registry), resolver)
}

fn normalize_allowlist_entry(entry: &str, field: &str) -> Result<String, String> {
    let normalized = entry.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        Err(format!("{field} must not contain empty values"))
    } else {
        Ok(normalized)
    }
}

fn normalize_request_allowlist(
    entries: Option<&[String]>,
    field: &str,
) -> Result<Option<HashSet<String>>, String> {
    let Some(entries) = entries else {
        return Ok(None);
    };
    let mut normalized = HashSet::new();
    for entry in entries {
        normalized.insert(normalize_allowlist_entry(entry, field)?);
    }
    Ok(Some(normalized))
}

fn normalize_request_skill_sources(
    entries: Option<&[String]>,
    field: &str,
) -> Result<Option<HashSet<crate::skills::manifest::SkillSourceKind>>, String> {
    let Some(entries) = entries else {
        return Ok(None);
    };
    let mut normalized = HashSet::new();
    for entry in entries {
        normalized.insert(
            entry
                .parse::<crate::skills::manifest::SkillSourceKind>()
                .map_err(|error| format!("{field}: {error}"))?,
        );
    }
    Ok(Some(normalized))
}

fn apply_normalized_skill_allowlist(
    resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
    request_constraints: &RequestConstraints,
) -> Result<Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>, String> {
    crate::turn::skill_tool::apply_skill_surfacing_policy(
        resolver,
        &request_constraints.skill_surfacing_policy(),
    )
}

/// Build a server-side skill executor that supports both Inline and Fork
/// execution contexts via [`SkillExecutionRouter`].
fn build_server_skill_executor(
    matrixone: &MatrixOneSettings,
    encryptor: &Arc<FernetTokenEncryptor>,
    shared_pool: Option<&SharedPool>,
    model_override: Option<&str>,
    admitted_model_execution: Option<&astra_services::AdmittedModelExecution>,
    edge_tools: &[Value],
    edge_profile: &Map<String, Value>,
    execution_bindings: Option<&ExecutionBindingSnapshot>,
    forward_headers: &HashMap<String, String>,
    request_constraints: RequestConstraints,
    inherited_permissions: InheritedPermissions,
    skill_resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
    reflect_service: Arc<dyn astra_services::ReflectService>,
    session_id: &str,
    edge_connection_pool: Option<&astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    memory_extraction_service: Option<&Arc<crate::session_memory::MemoryExtractionService>>,
    interaction_sink: Option<Arc<dyn server_loop_host::HostInteractionSink>>,
    #[cfg(feature = "harness")] harness_sink: Option<
        &std::sync::Arc<dyn astra_harness::SnapshotSink>,
    >,
) -> Option<Arc<dyn crate::skills::traits::SkillExecutor>> {
    use crate::server::server_skill_subrun::ServerSkillSubRunExecutor;
    use astra_skills::executor::isolated::{IsolatedSkillExecutor, SkillExecutionRouter};

    let mut subrun_executor = ServerSkillSubRunExecutor::new(
        matrixone.clone(),
        Arc::clone(encryptor),
        session_id.to_string(),
    )
    .with_pool(shared_pool.cloned())
    .with_default_model(model_override.map(String::from))
    .with_admitted_model_execution(admitted_model_execution.cloned())
    .with_edge_tools(edge_tools.to_vec())
    .with_edge_profile(edge_profile.clone())
    .with_forward_headers(forward_headers.clone())
    .with_request_constraints(request_constraints)
    .with_inherited_permissions(inherited_permissions)
    .with_skill_resolver(skill_resolver)
    .with_reflect_service(reflect_service)
    .with_cancel_token(cancel_token);
    if let Some(sink) = interaction_sink {
        subrun_executor = subrun_executor.with_interaction_sink(sink);
    }
    if let Some(snapshot) = execution_bindings {
        subrun_executor = subrun_executor.with_execution_binding_snapshot(snapshot.clone());
    }
    if let Some(svc) = memory_extraction_service {
        subrun_executor = subrun_executor.with_memory_extraction_service(Arc::clone(svc));
    }
    if let Some(pool) = edge_connection_pool {
        subrun_executor = subrun_executor.with_edge_connection_pool(pool.clone());
    }
    #[cfg(feature = "harness")]
    if let Some(sink) = harness_sink {
        subrun_executor = subrun_executor.with_harness_sink(Some(sink.clone()));
    }

    // Wire skill checkpoint manager for crash recovery resume.
    // This allows skills to resume from their last checkpoint instead of starting over.
    #[cfg(feature = "crash-recovery")]
    let isolated = {
        let checkpoint_dir = astra_services::session_journal::journal_file_path(session_id)
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("skill_checkpoints");
        let checkpoint_manager = Arc::new(TokioMutex::new(
            astra_pipeline::skill_checkpoint::SkillCheckpointManager::new(checkpoint_dir),
        ));
        let isolated = IsolatedSkillExecutor::with_checkpoint_manager(
            Arc::new(subrun_executor),
            checkpoint_manager,
        );
        Arc::new(isolated)
    };
    #[cfg(not(feature = "crash-recovery"))]
    let isolated = Arc::new(IsolatedSkillExecutor::new(Arc::new(subrun_executor)));

    let router = SkillExecutionRouter::new(Some(isolated));
    Some(Arc::new(router))
}

pub(crate) fn has_turn_verdict_warning(
    verdict_events: &[astra_turn_core::agentic_verdict_audit::AgenticVerdictAuditEvent],
) -> bool {
    verdict_events.iter().any(|event| {
        event.severity.eq_ignore_ascii_case("warning")
            || event.severity.eq_ignore_ascii_case("critical")
    })
}

fn build_runtime_turn_evaluation_event(
    session_id: &str,
    source: &str,
    state: &AgenticLoopState,
) -> astra_services::session_journal::JournalEvent {
    let verdict_warning = has_turn_verdict_warning(&state.stall.verdict_events);
    let eval_thresholds = crate::pipeline::evaluation::current_evaluation_thresholds();
    let eval = crate::pipeline::evaluation::evaluate_tool_call_records_with_thresholds(
        &state.message,
        &state.recent_tools,
        &state.stall.tool_call_records,
        state.stall.events.len(),
        verdict_warning,
        state.telemetry.first_budget_pressure,
        eval_thresholds,
    );
    crate::pipeline::evaluation::build_turn_evaluation_journal_event(
        Some(session_id),
        None,
        source,
        &state.message,
        &state.recent_tools,
        &state.stall.tool_call_records,
        state.stall.events.len(),
        verdict_warning,
        state.telemetry.first_budget_pressure,
        &eval,
    )
}

fn persist_turn_evaluation_journal(session_id: &str, source: &str, state: &AgenticLoopState) {
    if session_id.is_empty() {
        return;
    }

    let event = build_runtime_turn_evaluation_event(session_id, source, state);
    match astra_services::session_journal::JournalWriter::new(session_id) {
        Ok(journal) => {
            if let Err(err) = journal.append(&event) {
                tracing::warn!(
                    target: "astra_runtime::run_lifecycle",
                    session_id = %session_id,
                    error = %err,
                    "turn evaluation journal append failed"
                );
            }
        }
        Err(err) => tracing::warn!(
            target: "astra_runtime::run_lifecycle",
            session_id = %session_id,
            error = %err,
            "turn evaluation journal init failed"
        ),
    }
}

/// Best-effort flush of turn observability events to local journal.
fn flush_turn_observability(state: &mut AgenticLoopState, session_id: &str, interrupted: bool) {
    let Some(buf) = state.turn_event_buffer.as_mut() else {
        return;
    };
    if buf.is_empty() {
        return;
    }
    let Ok(writer) = astra_services::session_journal::JournalWriter::new(session_id) else {
        tracing::warn!(
            session_id,
            "flush_turn_observability: failed to create journal writer"
        );
        return;
    };
    if interrupted {
        if let Err(e) = buf.flush_interrupted(&writer) {
            tracing::warn!(
                session_id,
                error = %e,
                "flush_turn_observability: flush_interrupted failed"
            );
        }
    } else if let Err(e) = buf.flush(&writer) {
        tracing::warn!(
            session_id,
            error = %e,
            "flush_turn_observability: flush failed"
        );
    }
}

fn build_runtime_evaluation_service(
    matrixone: &MatrixOneSettings,
    shared_pool: &SharedPool,
) -> DatabaseEvaluationService {
    DatabaseEvaluationService::new(matrixone.clone()).with_pool(shared_pool.clone())
}

async fn initialize_runtime_controllers(
    loop_state: &mut AgenticLoopState,
    user_id: &str,
    session_id: &str,
    evaluation_persistence: Option<EvaluationPersistenceContext>,
    context_trace_persistence: Option<ContextTracePersistenceContext>,
) {
    let hub = Arc::new(ObservabilityHub::new());
    let session = hub.start_session(user_id, session_id);

    loop_state.telemetry.observability_hub = Some(hub);
    loop_state.telemetry.observability_session = Some(session);
    loop_state.telemetry.evaluation_persistence = evaluation_persistence;
    loop_state.telemetry.context_trace_persistence = context_trace_persistence;
}

async fn configure_runtime_controllers(
    matrixone: &MatrixOneSettings,
    shared_pool: Option<&SharedPool>,
    loop_state: &mut AgenticLoopState,
    user_id: &str,
    session_id: &str,
) {
    let evaluation_persistence = shared_pool.map(|pool| EvaluationPersistenceContext {
        user_id: user_id.to_string(),
        evaluation_service: build_runtime_evaluation_service(matrixone, pool),
    });
    let context_trace_persistence = shared_pool.map(|pool| ContextTracePersistenceContext {
        user_id: user_id.to_string(),
        event_service: build_runtime_event_service(matrixone, pool),
        artifact_store: astra_services::DatabaseSessionArtifactStore::new(matrixone.clone())
            .with_pool(pool.clone()),
        agent_id: RUNTIME_CONTEXT_TRACE_AGENT_ID.to_string(),
    });
    initialize_runtime_controllers(
        loop_state,
        user_id,
        session_id,
        evaluation_persistence,
        context_trace_persistence,
    )
    .await
}

fn build_runtime_event_service(
    matrixone: &MatrixOneSettings,
    shared_pool: &SharedPool,
) -> DatabaseEventService {
    DatabaseEventService::new(matrixone.clone()).with_pool(shared_pool.clone())
}

async fn persist_runtime_promotion_events(
    matrixone: &MatrixOneSettings,
    shared_pool: Option<&SharedPool>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    promotions: &[RuntimePromotionEventData],
) -> Result<(), String> {
    if promotions.is_empty() {
        return Ok(());
    }
    let Some(pool) = shared_pool else {
        tracing::debug!(
            session_id,
            "runtime promotion persistence skipped: shared_pool not configured"
        );
        return Ok(());
    };

    let service = build_runtime_event_service(matrixone, pool);
    for promotion in promotions {
        let metadata = match serde_json::to_value(promotion) {
            Ok(value) => Some(value),
            Err(err) => {
                tracing::warn!(
                    target: "astra_runtime::run_lifecycle",
                    subject_id = %promotion.subject_id,
                    error = %err,
                    "runtime promotion event serialize failed"
                );
                continue;
            }
        };
        if let Err((status, response)) = service
            .create_event(
                user_id.to_string(),
                EventCreateRequestData {
                    ingestion_source: astra_services::events::EventIngestionSource::Client,
                    event_id: None,
                    session_id: session_id.to_string(),
                    event_type: RUNTIME_PROMOTION_EVENT_TYPE.to_string(),
                    content: promotion.summary.clone(),
                    agent_id: None,
                    agent_version: None,
                    parent_event_id: None,
                    parent_event_ids: Some(Vec::new()),
                    causal_chain_id: Some(format!(
                        "{session_id}:runtime-promotion:{}:{run_id}",
                        promotion.subject_id
                    )),
                    metadata,
                },
            )
            .await
        {
            tracing::warn!(
                target: "astra_runtime::run_lifecycle",
                subject_id = %promotion.subject_id,
                status = %status,
                detail = %response.0.detail,
                "runtime promotion event persist failed"
            );
        }
    }
    Ok(())
}

pub(crate) use persistence::{
    PostLoopPersistContext, TranscriptPersistItem, TranscriptPersistPayload,
    build_run_turn_complete_event_with_interruption, format_task_board_resume_hint,
    materialize_server_run_transcript_evidence, persist_server_loop_core_events,
    persist_server_loop_trace_events, persist_server_loop_transcript_items,
    persist_session_transcript_items, persist_session_transcript_items_inner_in_tx,
    restore_session_state_compact, restore_step_checkpoint_runtime_state,
    server_loop_causal_chain_id, server_trace_context, trace_context_from_subrun_context,
};
use run_state::*;

#[cfg(test)]
async fn stream_missing_agent_lifecycle_events(
    spawner: &DynamicAgentSpawner,
    root_run_id: &str,
    event_tx: &mpsc::Sender<Value>,
    sent_lifecycle_events: &AgentProgressLifecycleLedger,
) -> bool {
    let events =
        collect_missing_agent_lifecycle_events(spawner, root_run_id, sent_lifecycle_events).await;
    for event in events {
        if event_tx.send(event).await.is_err() {
            return false;
        }
    }
    true
}

#[derive(Clone)]
struct ServerAgentSpawnerEntry {
    spawner: Arc<DynamicAgentSpawner>,
    executor: Arc<ServerSpawnAgentExecutor>,
    active_work_registry: Arc<astra_core::work_unit::ActiveWorkRegistry>,
    durable_restore: Arc<tokio::sync::OnceCell<()>>,
    last_access: Arc<std::sync::Mutex<Instant>>,
}

const SERVER_AGENT_SPAWNER_IDLE_TTL: Duration = Duration::from_secs(15 * 60);
const SERVER_AGENT_SPAWNER_PRUNE_BATCH: usize = 32;

impl ServerAgentSpawnerEntry {
    fn touch(&self) {
        *self
            .last_access
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Instant::now();
    }

    fn idle_for(&self, now: Instant) -> Duration {
        now.saturating_duration_since(
            *self
                .last_access
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }
}

struct ServerDurableAgentReconciler {
    run_engine: RunEngine,
    user_id: String,
    session_id: String,
    state: TokioMutex<ServerDurableAgentReconcileState>,
}

#[derive(Default)]
struct ServerDurableAgentReconcileState {
    last_attempt: Option<Instant>,
    cached: Option<Result<Vec<DurableRunRecord>, String>>,
}

#[async_trait]
impl DurableAgentReconciler for ServerDurableAgentReconciler {
    async fn load_agent_recovery(&self) -> Result<Vec<DurableRunRecord>, String> {
        const MIN_REFRESH_INTERVAL: Duration = Duration::from_millis(500);
        let mut state = self.state.lock().await;
        if state
            .last_attempt
            .is_some_and(|attempt| attempt.elapsed() < MIN_REFRESH_INTERVAL)
            && let Some(cached) = &state.cached
        {
            return cached.clone();
        }
        let result = self
            .run_engine
            .load_session_agent_recovery(&self.user_id, &self.session_id, 200)
            .await
            .map(|page| page.runs);
        state.last_attempt = Some(Instant::now());
        state.cached = Some(result.clone());
        result
    }
}

#[derive(Clone)]
struct ResolvedAgentBindingRuntime {
    binding: astra_services::AgentBindingRecord,
    mcp_server: astra_services::CapabilityServerEndpoint,
    skill_server: astra_services::CapabilityServerEndpoint,
}

#[derive(Clone, Default)]
struct PreparedRuntimeCapabilities {
    mcp_bundle: Option<runtime_mcp::RuntimeMcpBundle>,
    request_scoped_skill_resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
    agent_binding: Option<PreparedAgentBindingLoopContext>,
}

#[derive(Clone)]
struct PreparedAgentBindingLoopContext {
    binding: astra_services::AgentBindingRecord,
    skill_resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
}

#[derive(Clone)]
struct ServerSpawnRuntimeContext {
    parent_run_id: String,
    user_id: String,
    session_id: String,
    trace_context: TraceContext,
    forward_headers: HashMap<String, String>,
    admitted_model_execution: Option<astra_services::AdmittedModelExecution>,
    request_constraints: RequestConstraints,
    execution_metadata: Option<Value>,
    /// The session-owned dynamic-agent lifecycle.  Kept weak here because
    /// the spawner already owns this executor; retaining it would create an
    /// executor → context → spawner → executor reference cycle for every
    /// run.  A live sub-run upgrades it only while installing its own tool
    /// context.
    spawner: Weak<DynamicAgentSpawner>,
    pause_flag: Option<Arc<AtomicBool>>,
    cancel_token: Option<Arc<CancellationToken>>,
    #[cfg(feature = "bridge-e2e-hooks")]
    test_child_llm_rounds: Vec<Value>,
    #[cfg(feature = "harness")]
    harness_sink: Option<Arc<dyn astra_harness::SnapshotSink>>,
}

// ─── Service ────────────────────────────────────────────────────────────────

/// Spawn a fire-and-forget background task. Unlike a raw `tokio::spawn` whose
/// `JoinHandle` is silently dropped, this wrapper catches panics inside the
/// spawned future and emits a `tracing::error` log so that silent failures
/// are observable.
pub(crate) fn spawn_observed(
    future: impl std::future::Future<Output = ()> + Send + 'static,
    name: &'static str,
) {
    tokio::spawn(async move {
        let result = AssertUnwindSafe(future).catch_unwind().await;
        if let Err(panic_err) = result {
            let msg = panic_err
                .downcast_ref::<String>()
                .map(|s| s.as_str())
                .or_else(|| panic_err.downcast_ref::<&str>().copied())
                .unwrap_or("unknown panic");
            tracing::error!(task = name, panic = %msg, "background task panicked");
        }
    });
}

struct ActiveRunControlWatcher {
    stop_tx: Option<oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<()>,
}

impl Drop for ActiveRunControlWatcher {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        self.join.abort();
    }
}

fn start_active_run_control_watcher(
    run_control: Option<Arc<dyn RunControlProvider>>,
    user_id: String,
    run_id: String,
    cancel_flag: Arc<AtomicBool>,
    pause_flag: Arc<AtomicBool>,
    cancel_token: Arc<CancellationToken>,
) -> Option<ActiveRunControlWatcher> {
    let run_control = run_control?;
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    let join = tokio::spawn(async move {
        let mut poll = tokio::time::interval_at(
            tokio::time::Instant::now() + ACTIVE_RUN_DURABLE_CONTROL_WATCH_INTERVAL,
            ACTIVE_RUN_DURABLE_CONTROL_WATCH_INTERVAL,
        );
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                _ = poll.tick() => {
                    if cancel_flag.load(Ordering::Acquire) || cancel_token.is_cancelled() {
                        break;
                    }
                    let control_status = tokio::time::timeout(
                        ACTIVE_RUN_DURABLE_CONTROL_POLL_TIMEOUT,
                        run_control.control_status(&user_id, &run_id),
                    )
                    .await;
                    match control_status {
                        Err(_) => {
                            tracing::warn!(
                                target: "astra_runtime::run_lifecycle",
                                run_id = %run_id,
                                timeout_ms = ACTIVE_RUN_DURABLE_CONTROL_POLL_TIMEOUT.as_millis() as u64,
                                "active run control watcher durable status poll timed out"
                            );
                        }
                        Ok(Ok(Some(RunControlStatus::Cancelled))) => {
                            cancel_flag.store(true, Ordering::SeqCst);
                            cancel_token.cancel();
                            break;
                        }
                        Ok(Ok(Some(RunControlStatus::Paused))) => {
                            pause_flag.store(true, Ordering::SeqCst);
                        }
                        Ok(Ok(None)) => {
                            // Resume is a durable fact. Clear only this run's
                            // private flag; dynamic children never share a
                            // parent's pause flag, so a child cannot unpause
                            // unrelated work.
                            pause_flag.store(false, Ordering::SeqCst);
                        }
                        Ok(Err(error)) => {
                            tracing::warn!(
                                target: "astra_runtime::run_lifecycle",
                                run_id = %run_id,
                                error = %error,
                                "active run control watcher failed to poll durable status"
                            );
                        }
                    }
                }
            }
        }
    });
    Some(ActiveRunControlWatcher {
        stop_tx: Some(stop_tx),
        join,
    })
}

/// Production [`RunLifecycleService`] that executes agentic loops via
/// [`ServerAgenticLoopHost`].
///
/// Durable run state is mandatory; process-local state is limited to live
/// control handles that cannot survive a restart.
pub struct AgenticRunLifecycleService {
    /// Process-local run handles (run_id -> state) for live cancellation, pause,
    /// and active SSE fanout. Durable state is the user-visible authority.
    runs: Arc<RwLock<HashMap<String, RunState>>>,
    /// LLM resolution dependencies.
    matrixone: MatrixOneSettings,
    encryptor: Arc<FernetTokenEncryptor>,
    shared_pool: Option<SharedPool>,
    /// Edge callback ledger shared with the API server.
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    /// P0-3: Cross-pod edge dispatch service. When configured, tool results
    /// delivered to another pod are visible via DB polling fallback.
    edge_dispatch_service: Option<Arc<dyn astra_services::multi_agent::EdgeDispatchService>>,
    /// P0-3: Edge registry for cross-pod edge agent discovery.
    edge_registry_service: Option<Arc<dyn astra_services::multi_agent::EdgeRegistryService>>,
    /// Durable run engine for persistence, replay, status, and recovery.
    run_engine: RunEngine,
    /// Optional delegation engine for multi-agent coordination.
    delegation_engine: Option<Arc<crate::server::delegation::engine::DelegationEngine>>,
    /// Session-scoped dynamic-agent spawners used by Web/server `agent(action='spawn')`.
    server_agent_spawners: Arc<RwLock<HashMap<String, ServerAgentSpawnerEntry>>>,
    /// Fallback progress broadcaster for dynamic spawn when no delegation
    /// engine is configured. Normal production wiring uses the delegation
    /// engine broadcaster so Web SSE sees one agent tree stream.
    server_agent_progress_broadcaster: Arc<ProgressBroadcaster>,
    /// Shared mailbox router for Web/server dynamic spawned agents.
    server_agent_mailbox_router: Arc<astra_messaging::router::AgentMailboxRouter>,
    /// Per-user resource governor (Phase 5).
    resource_governor:
        Option<std::sync::Arc<dyn astra_services::resource_governor::ResourceGovernor>>,
    /// Live edge WebSocket connection pool (Phase 6).
    edge_connection_pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    /// Durable workspace record store for cloud workspace ownership/audit.
    workspace_record_store: Option<Arc<dyn WorkspaceStateStore>>,
    /// Optional database skill provider for runtime skill resolution.
    skill_service: Option<Arc<dyn SkillService>>,
    /// Exact model catalog used to resolve client-visible Offering IDs.
    model_service: Arc<dyn ModelService>,
    /// Registry-backed MCP bindings available to server-side chat loops.
    mcp_registry_service: Arc<dyn astra_services::McpRegistryService>,
    /// Immutable Agent Binding snapshots for binding-backed chat loops.
    agent_binding_service: Arc<dyn astra_services::AgentBindingService>,
    /// Optional model gateway registry for per-turn model resolution.
    model_gateway_service: Arc<dyn astra_services::ModelGatewayService>,
    /// Per-run approval request channel receivers (Phase E).
    /// Key: run_id → receiver that the WS handler drains.
    approval_channels: Arc<TokioMutex<HashMap<String, mpsc::Receiver<serde_json::Value>>>>,
    /// Per-run ask_user prompt channel receivers.
    /// Key: run_id → receiver that the WS handler drains.
    user_prompt_channels: Arc<TokioMutex<HashMap<String, mpsc::Receiver<serde_json::Value>>>>,
    /// Per-run progress event channel receivers (Phase F.3).
    /// Key: run_id → receiver that the WS handler drains.
    progress_channels: Arc<TokioMutex<HashMap<String, mpsc::Receiver<ProgressEvent>>>>,
    /// Hook DB writer for decision audit + skill selection persistence.
    hook_db_writer: Option<Arc<dyn TurnHookDbWriter>>,
    /// Memoria observer worker for cross-session knowledge extraction.
    observer_worker: Option<Arc<dyn TurnObserverWorker>>,
    /// Tool event writer for persisting tool_call events to agent_events.
    tool_event_writer: Option<Arc<dyn TurnToolEventWriter>>,
    /// Auxiliary event writer for ask_user lifecycle audit events.
    auxiliary_event_writer: Option<Arc<dyn crate::TurnAuxiliaryEventWriter>>,
    /// Counter of in-flight background agentic loop tasks.
    /// Incremented before spawn, decremented when the task exits.
    /// Used by `drain_background_tasks` for graceful shutdown.
    background_task_count: Arc<AtomicUsize>,
    /// Global admission control: limits the number of concurrently executing
    /// agentic loop tasks across all users. A permit is acquired before
    /// spawn and automatically released when the task completes.
    run_semaphore: Arc<tokio::sync::Semaphore>,
    /// Shared metrics registry for capacity/admission signals exposed via /metrics.
    metrics_registry: Option<Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>>,
    /// Harness sink registry for server-side harness observation (Phase 2A).
    #[cfg(feature = "harness")]
    harness_registry: Option<crate::server::harness::handlers::HarnessSinkRegistry>,
    /// Shared background session-memory extraction coordinator. Cloned
    /// into every `AgenticLoopState` the service builds, so all turns
    /// share selector cooldown, in-flight dedup, event sink, and
    /// broker. `None` → extraction disabled (e.g. minimal test service).
    memory_extraction_service: Option<Arc<crate::session_memory::MemoryExtractionService>>,
    /// Shared ToolExecutionService so executors share the same disabled_tool_offers set.
    tool_execution_service: Option<ToolExecutionService>,
    /// Shared persisted-reflection service used by the visible `reflect` tool.
    reflect_service: Arc<dyn astra_services::ReflectService>,
}

impl AgenticRunLifecycleService {
    pub fn new(
        matrixone: MatrixOneSettings,
        encryptor: Arc<FernetTokenEncryptor>,
        edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
        run_engine: RunEngine,
    ) -> Self {
        Self {
            runs: Arc::new(RwLock::new(HashMap::new())),
            matrixone,
            encryptor,
            shared_pool: None,
            edge_callback_ledger,
            edge_dispatch_service: None,
            edge_registry_service: None,
            run_engine,
            delegation_engine: None,
            server_agent_spawners: Arc::new(RwLock::new(HashMap::new())),
            server_agent_progress_broadcaster: Arc::new(ProgressBroadcaster::default()),
            server_agent_mailbox_router: Arc::new(astra_messaging::AgentMailboxRouter::new(
                Arc::new(astra_messaging::InProcessTransport::new()),
                Arc::new(crate::server::delegation::engine::DelegationTracker::new()),
            )),
            resource_governor: None,
            edge_connection_pool: None,
            workspace_record_store: None,
            skill_service: None,
            model_service: Arc::new(astra_services::UnconfiguredModelService),
            mcp_registry_service: Arc::new(astra_services::UnconfiguredMcpRegistryService),
            agent_binding_service: Arc::new(astra_services::UnconfiguredAgentBindingService),
            model_gateway_service: Arc::new(astra_services::UnconfiguredModelGatewayService),
            approval_channels: Arc::new(TokioMutex::new(HashMap::new())),
            user_prompt_channels: Arc::new(TokioMutex::new(HashMap::new())),
            progress_channels: Arc::new(TokioMutex::new(HashMap::new())),
            hook_db_writer: None,
            observer_worker: None,
            tool_event_writer: None,
            auxiliary_event_writer: None,
            background_task_count: Arc::new(AtomicUsize::new(0)),
            run_semaphore: Arc::new(tokio::sync::Semaphore::new(50)),
            metrics_registry: None,
            #[cfg(feature = "harness")]
            harness_registry: None,
            memory_extraction_service: None,
            tool_execution_service: None,
            reflect_service: Arc::new(astra_services::UnconfiguredReflectService),
        }
    }

    pub fn with_memory_extraction_service(
        mut self,
        svc: Arc<crate::session_memory::MemoryExtractionService>,
    ) -> Self {
        self.memory_extraction_service = Some(svc);
        self
    }

    #[cfg(feature = "harness")]
    pub fn with_harness_registry(
        mut self,
        registry: crate::server::harness::handlers::HarnessSinkRegistry,
    ) -> Self {
        self.harness_registry = Some(registry);
        self
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.shared_pool = Some(pool);
        self
    }

    pub fn with_agent_mailbox_router(
        mut self,
        router: Arc<astra_messaging::router::AgentMailboxRouter>,
    ) -> Self {
        self.server_agent_mailbox_router = router;
        self
    }

    pub fn with_delegation_engine(
        mut self,
        engine: Arc<crate::server::delegation::engine::DelegationEngine>,
    ) -> Self {
        self.delegation_engine = Some(engine);
        self
    }

    pub fn with_edge_connection_pool(
        mut self,
        pool: astra_server_types::edge_connection_pool::EdgeConnectionPool,
    ) -> Self {
        self.edge_connection_pool = Some(pool);
        self
    }

    /// P0-3: Wire the cross-pod edge dispatch service for horizontal scaling.
    pub fn with_edge_dispatch_service(
        mut self,
        svc: Arc<dyn astra_services::multi_agent::EdgeDispatchService>,
    ) -> Self {
        self.edge_dispatch_service = Some(svc);
        self
    }

    pub fn with_edge_registry_service(
        mut self,
        svc: Arc<dyn astra_services::multi_agent::EdgeRegistryService>,
    ) -> Self {
        self.edge_registry_service = Some(svc);
        self
    }

    pub(crate) fn with_workspace_record_store(
        mut self,
        store: Arc<dyn WorkspaceStateStore>,
    ) -> Self {
        self.workspace_record_store = Some(store);
        self
    }

    pub fn with_tool_execution_service(mut self, service: ToolExecutionService) -> Self {
        self.tool_execution_service = Some(service);
        self
    }

    pub fn with_reflect_service(
        mut self,
        service: Arc<dyn astra_services::ReflectService>,
    ) -> Self {
        self.reflect_service = service;
        self
    }

    pub fn with_resource_governor(
        mut self,
        governor: std::sync::Arc<dyn astra_services::resource_governor::ResourceGovernor>,
    ) -> Self {
        self.resource_governor = Some(governor);
        self
    }

    pub fn with_skill_service(mut self, service: Arc<dyn SkillService>) -> Self {
        self.skill_service = Some(service);
        self
    }

    pub fn with_model_service(mut self, service: Arc<dyn ModelService>) -> Self {
        self.model_service = service;
        self
    }

    pub fn with_mcp_registry_service(
        mut self,
        service: Arc<dyn astra_services::McpRegistryService>,
    ) -> Self {
        self.mcp_registry_service = service;
        self
    }

    pub fn with_agent_binding_service(
        mut self,
        service: Arc<dyn astra_services::AgentBindingService>,
    ) -> Self {
        self.agent_binding_service = service;
        self
    }

    pub fn with_model_gateway_service(
        mut self,
        service: Arc<dyn astra_services::ModelGatewayService>,
    ) -> Self {
        self.model_gateway_service = service;
        self
    }

    pub fn with_hook_db_writer(mut self, writer: Arc<dyn TurnHookDbWriter>) -> Self {
        self.hook_db_writer = Some(writer);
        self
    }

    pub fn with_observer_worker(mut self, worker: Arc<dyn TurnObserverWorker>) -> Self {
        self.observer_worker = Some(worker);
        self
    }

    pub fn with_tool_event_writer(mut self, writer: Arc<dyn TurnToolEventWriter>) -> Self {
        self.tool_event_writer = Some(writer);
        self
    }

    pub fn with_auxiliary_event_writer(
        mut self,
        writer: Arc<dyn crate::TurnAuxiliaryEventWriter>,
    ) -> Self {
        self.auxiliary_event_writer = Some(writer);
        self
    }

    /// Configure the maximum number of concurrent agentic loop tasks.
    /// Default: 50. Set via env `ASTRA_RUN_CONCURRENCY_LIMIT`.
    pub fn with_run_concurrency_limit(mut self, limit: usize) -> Self {
        self.run_semaphore = Arc::new(tokio::sync::Semaphore::new(limit));
        self
    }

    pub fn with_metrics_registry(
        mut self,
        registry: Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>,
    ) -> Self {
        register_run_admission_metrics(&registry);
        register_durable_run_event_metrics(&registry);
        self.metrics_registry = Some(registry);
        self
    }

    #[cfg(test)]
    pub(crate) fn test_run_semaphore(&self) -> Arc<tokio::sync::Semaphore> {
        self.run_semaphore.clone()
    }

    async fn remove_run_channels(&self, run_id: &str) {
        self.approval_channels.lock().await.remove(run_id);
        self.user_prompt_channels.lock().await.remove(run_id);
        self.progress_channels.lock().await.remove(run_id);
    }

    async fn acquire_run_permit(
        &self,
        timeout: Duration,
    ) -> Result<OwnedSemaphorePermit, RunAdmissionError> {
        let start = Instant::now();
        match tokio::time::timeout(timeout, self.run_semaphore.clone().acquire_owned()).await {
            Ok(Ok(permit)) => {
                self.record_run_admission("acquired", start.elapsed());
                Ok(permit)
            }
            Ok(Err(_closed)) => {
                self.record_run_admission("closed", start.elapsed());
                Err(RunAdmissionError::Closed)
            }
            Err(_elapsed) => {
                self.record_run_admission("timeout", start.elapsed());
                Err(RunAdmissionError::Timeout)
            }
        }
    }

    fn record_run_admission(&self, outcome: &'static str, wait: Duration) {
        let Some(registry) = self.metrics_registry.as_ref() else {
            return;
        };
        register_run_admission_metrics(registry);
        registry.increment_counter(
            METRIC_RUN_ADMISSION_ATTEMPTS_TOTAL,
            &[("outcome", outcome)],
            1,
        );
        registry.increment_counter(
            METRIC_RUN_ADMISSION_WAIT_MS_TOTAL,
            &[("outcome", outcome)],
            duration_millis_u64(wait),
        );
    }

    fn dynamic_agent_progress_broadcaster(&self) -> Arc<ProgressBroadcaster> {
        self.delegation_engine
            .as_ref()
            .and_then(|engine| engine.progress_broadcaster().cloned())
            .unwrap_or_else(|| Arc::clone(&self.server_agent_progress_broadcaster))
    }

    fn spawn_agent_progress_stream_bridge(
        &self,
        root_run_id: String,
        event_tx: mpsc::Sender<Value>,
    ) -> AgentProgressStreamBridge {
        let mut progress_rx = self.dynamic_agent_progress_broadcaster().subscribe();
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        let sent_lifecycle_events = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let sent_lifecycle_events_for_bridge = Arc::clone(&sent_lifecycle_events);
        let join = tokio::spawn(async move {
            let mut filter = server_loop_host::RunScopedAgentProgressFilter::new(root_run_id);
            'bridge: loop {
                tokio::select! {
                    _ = &mut stop_rx => {
                        if !drain_ready_agent_progress_events(
                            &mut progress_rx,
                            &mut filter,
                            &event_tx,
                            &sent_lifecycle_events_for_bridge,
                        )
                        .await
                        {
                            break 'bridge;
                        }
                        let drain_deadline = tokio::time::sleep(AGENT_PROGRESS_STREAM_DRAIN_GRACE);
                        tokio::pin!(drain_deadline);
                        'drain: loop {
                            tokio::select! {
                                _ = &mut drain_deadline => break 'drain,
                                received = progress_rx.recv() => {
                                    match received {
                                        Ok(evt) => {
                                            if !forward_agent_progress_event_to_stream(
                                                &mut filter,
                                                &event_tx,
                                                &sent_lifecycle_events_for_bridge,
                                                evt,
                                            )
                                            .await
                                            {
                                                break 'bridge;
                                            }
                                        }
                                        Err(broadcast::error::RecvError::Lagged(dropped)) => {
                                            tracing::warn!(
                                                target: "astra_runtime::work_surface",
                                                dropped,
                                                "agent progress live stream lagged while draining"
                                            );
                                            continue;
                                        }
                                        Err(broadcast::error::RecvError::Closed) => break 'drain,
                                    }
                                }
                            }
                        }
                        break;
                    },
                    received = progress_rx.recv() => {
                        match received {
                            Ok(evt) => {
                                if !forward_agent_progress_event_to_stream(
                                    &mut filter,
                                    &event_tx,
                                    &sent_lifecycle_events_for_bridge,
                                    evt,
                                )
                                .await
                                {
                                    break 'bridge;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(dropped)) => {
                                tracing::warn!(
                                    target: "astra_runtime::work_surface",
                                    dropped,
                                    "agent progress live stream lagged"
                                );
                                continue;
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
        });
        AgentProgressStreamBridge {
            stop_tx,
            join,
            sent_lifecycle_events,
        }
    }

    async fn server_agent_spawner_for_session(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> ServerAgentSpawnerEntry {
        self.prune_idle_server_agent_spawners().await;
        let registry_key = format!("{user_id}\0{session_id}");
        if let Some(entry) = self
            .server_agent_spawners
            .read()
            .await
            .get(&registry_key)
            .cloned()
        {
            entry.touch();
            return entry;
        }

        let mut guard = self.server_agent_spawners.write().await;
        if let Some(entry) = guard.get(&registry_key).cloned() {
            entry.touch();
            return entry;
        }

        let mut executor = ServerSpawnAgentExecutor::new(
            self.matrixone.clone(),
            Arc::clone(&self.encryptor),
            Arc::clone(&self.edge_callback_ledger),
        )
        .with_run_engine(self.run_engine.clone())
        .with_pool(self.shared_pool.clone())
        .with_edge_connection_pool(self.edge_connection_pool.clone())
        .with_skill_service(self.skill_service.clone())
        .with_memory_extraction_service(self.memory_extraction_service.clone())
        .with_reflect_service(Arc::clone(&self.reflect_service));
        if let Some(service) = self.edge_dispatch_service.clone() {
            executor = executor.with_edge_dispatch_service(service);
        }
        if let Some(service) = self.edge_registry_service.clone() {
            executor = executor.with_edge_registry_service(service);
        }
        let executor = Arc::new(executor);
        let executor_for_spawner: Arc<dyn SpawnAgentExecutor> = executor.clone();
        let active_work_registry = Arc::new(astra_core::work_unit::ActiveWorkRegistry::default());
        let mut spawner = DynamicAgentSpawner::with_broadcaster(
            Arc::clone(&self.server_agent_mailbox_router),
            self.dynamic_agent_progress_broadcaster(),
        )
        .with_executor(executor_for_spawner)
        .with_active_work_registry(active_work_registry.clone())
        .with_session(session_id.to_string())
        // Same cap as the CLI side. Web/headless sessions are no less
        // susceptible to the runaway-spawn-on-failure pattern; without
        // a cap, a misbehaving agent can fan out unbounded sub-agents
        // and burn the parent's quota.
        .with_max_concurrent_agents(resolved_server_spawn_concurrency_cap());
        if let Some(pool) = self.shared_pool.clone() {
            spawner = spawner.with_trace_writer(Arc::new(
                DatabaseTraceEventWriter::new(self.matrixone.clone()).with_pool(pool),
            ));
        }
        if let Some(store) = self
            .delegation_engine
            .as_ref()
            .and_then(|engine| engine.prefix_store().cloned())
        {
            spawner = spawner.with_prefix_store(store);
        }

        let entry = ServerAgentSpawnerEntry {
            spawner: Arc::new(spawner),
            executor,
            active_work_registry,
            durable_restore: Arc::new(tokio::sync::OnceCell::new()),
            last_access: Arc::new(std::sync::Mutex::new(Instant::now())),
        };
        guard.insert(registry_key, entry.clone());
        entry
    }

    async fn existing_server_agent_spawner_for_session(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Option<Arc<DynamicAgentSpawner>> {
        let registry_key = format!("{user_id}\0{session_id}");
        self.server_agent_spawners
            .read()
            .await
            .get(&registry_key)
            .map(|entry| Arc::clone(&entry.spawner))
    }

    async fn cancel_durable_run_descendants(
        run_engine: &RunEngine,
        user_id: &str,
        session_id: &str,
        parent_run_id: &str,
        reason: &str,
    ) -> Result<usize, String> {
        const PAGE_LIMIT: u32 = 200;
        const MAX_PAGES: usize = 32;
        let mut cancelled = 0;
        let mut cursor = None;

        for page_index in 0..MAX_PAGES {
            let page = run_engine
                .list_active_session_runs_cursor(user_id, session_id, PAGE_LIMIT, cursor.take())
                .await?;
            let next_cursor = page.next_cursor;
            let mut descendants = page
                .runs
                .into_iter()
                .filter(|run| {
                    run.run_id != parent_run_id
                        && run.ancestor_path.as_deref().is_some_and(|path| {
                            path.split('/').any(|segment| segment == parent_run_id)
                        })
                })
                .collect::<Vec<_>>();
            descendants.sort_by_key(|run| std::cmp::Reverse(run.depth));

            for descendant in descendants {
                let event = json!({
                    "event_type": "run_finished",
                    "data": {
                        "cancelled": true,
                        "reason": reason,
                        "source": "ancestor_run",
                        "ancestor_run_id": parent_run_id,
                    }
                });
                if run_engine
                    .transition_status_with_event_if_current(
                        user_id,
                        &descendant.run_id,
                        &[descendant.status.as_str()],
                        STATUS_CANCELLED,
                        None,
                        None,
                        event,
                    )
                    .await?
                {
                    cancelled += 1;
                }
            }
            let Some(next_cursor) = next_cursor else {
                return Ok(cancelled);
            };
            if page_index + 1 == MAX_PAGES {
                return Err(format!(
                    "active descendant cancellation exceeded {MAX_PAGES} pages for session {session_id}"
                ));
            }
            cursor = Some(next_cursor);
        }
        Ok(cancelled)
    }

    async fn converge_cancelled_run_descendants(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
    ) {
        if let Some(de) = &self.delegation_engine {
            de.cancel_children_of(run_id).await;
        }
        if let Some(spawner) = self
            .existing_server_agent_spawner_for_session(user_id, session_id)
            .await
        {
            let cancelled_children = spawner
                .cancel_descendants_of_parent_run(
                    run_id,
                    DescendantCancellationReason::AncestorCancelled,
                )
                .await;
            if cancelled_children > 0 {
                tracing::info!(
                    target: "astra_runtime::run_lifecycle",
                    run_id,
                    cancelled_children,
                    "cancel endpoint converged dynamic-agent descendants"
                );
            }
        }
        match Self::cancel_durable_run_descendants(
            &self.run_engine,
            user_id,
            session_id,
            run_id,
            DescendantCancellationReason::AncestorCancelled.as_str(),
        )
        .await
        {
            Ok(cancelled_children) if cancelled_children > 0 => {
                tracing::info!(
                    target: "astra_runtime::run_lifecycle",
                    run_id,
                    cancelled_children,
                    "cancel endpoint converged durable descendants"
                );
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    target: "astra_runtime::run_lifecycle",
                    run_id,
                    error = %error,
                    "cancel endpoint durable descendant sweep failed"
                );
            }
        }
    }

    async fn prune_idle_server_agent_spawners(&self) {
        let now = Instant::now();
        let candidates = self
            .server_agent_spawners
            .read()
            .await
            .iter()
            .filter(|(_, entry)| entry.idle_for(now) >= SERVER_AGENT_SPAWNER_IDLE_TTL)
            .take(SERVER_AGENT_SPAWNER_PRUNE_BATCH)
            .map(|(key, entry)| (key.clone(), entry.clone()))
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return;
        }

        let mut idle = Vec::new();
        for (key, entry) in candidates {
            if entry.spawner.background_task_count() == 0
                && entry.spawner.list_all_agents().await.is_empty()
            {
                idle.push((key, entry.spawner));
            }
        }
        if idle.is_empty() {
            return;
        }

        let mut registry = self.server_agent_spawners.write().await;
        for (key, observed_spawner) in idle {
            if registry
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(&current.spawner, &observed_spawner))
            {
                registry.remove(&key);
            }
        }
    }

    /// Single source of truth: parse all three allowlist lanes from raw wire
    /// shape, validating each. Every code path that needs a
    /// [`RequestConstraints`] for the agentic loop runs through this; the
    /// previous `.expect("validated before state build")` ladder is gone
    /// because validation and construction now happen together.
    fn try_request_constraints(request: &ChatRequestData) -> Result<RequestConstraints, String> {
        let enabled_tools =
            normalize_request_allowlist(request.enabled_tools.as_deref(), "enabled_tools")?
                .or_else(|| Some(HashSet::new()));
        Ok(RequestConstraints::new(
            normalize_request_allowlist(request.allow_tools.as_deref(), "allow_tools")?,
            enabled_tools,
            normalize_request_allowlist(request.allow_skills.as_deref(), "allow_skills")?,
            normalize_request_skill_sources(
                request.allow_skill_sources.as_deref(),
                "allow_skill_sources",
            )?,
        ))
    }

    fn root_permission_mode_from_request(request: &ChatRequestData) -> PermissionMode {
        match request.interaction_mode {
            Some(RequestedTurnInteractionMode::Deny) => PermissionMode::Deny,
            _ => PermissionMode::Auto,
        }
    }

    fn inherited_permissions_from_request(
        request: &ChatRequestData,
        constraints: &RequestConstraints,
    ) -> InheritedPermissions {
        let mut inherited =
            InheritedPermissions::new(Self::root_permission_mode_from_request(request));
        inherited.allowed_tools = constraints.allowed_tools.clone();
        inherited
    }

    #[allow(clippy::too_many_arguments)]
    async fn wire_server_dynamic_agent_tools(
        &self,
        executor: &mut runtime_tool_executor::RuntimeToolExecutor,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        turn_seq: u32,
        request: &ChatRequestData,
        workspace: &std::path::Path,
        work_surface_event_tx: Option<mpsc::Sender<Value>>,
        work_surface_gap_tracker: Option<WorkSurfaceAgentLiveGapTracker>,
        pause_flag: Option<Arc<AtomicBool>>,
        cancel_token: Option<Arc<CancellationToken>>,
        #[cfg(feature = "harness")] harness_sink: Option<Arc<dyn astra_harness::SnapshotSink>>,
    ) -> Arc<astra_core::work_unit::ActiveWorkRegistry> {
        let entry = self
            .server_agent_spawner_for_session(user_id, session_id)
            .await;
        if let Err(error) = entry
            .durable_restore
            .get_or_try_init(|| async {
                let reconciler = Arc::new(ServerDurableAgentReconciler {
                    run_engine: self.run_engine.clone(),
                    user_id: user_id.to_string(),
                    session_id: session_id.to_string(),
                    state: TokioMutex::new(ServerDurableAgentReconcileState::default()),
                });
                entry
                    .spawner
                    .set_durable_agent_reconciler(reconciler.clone())
                    .await;
                let runs = reconciler.load_agent_recovery().await?;
                entry.spawner.restore_durable_agent_runs(&runs).await;
                Ok::<(), String>(())
            })
            .await
        {
            tracing::warn!(
                %user_id,
                %session_id,
                %error,
                "durable agent registry restore failed; the next turn will retry"
            );
        }
        for observation in entry.spawner.active_fanout_work_unit_observations().await {
            entry.active_work_registry.observe(&observation);
        }
        // Validation already happened up the call chain (see
        // `validate_request_constraints`); this re-parse is safe because the
        // wire-level shape was checked before this point. If validation ever
        // becomes optional on this path, the `unwrap_or_else` below logs the
        // surprise instead of silently building corrupt constraints.
        let request_constraints = Self::try_request_constraints(request).unwrap_or_else(|err| {
            tracing::error!(error = %err, "request constraints failed late validation in dynamic-agent wiring");
            RequestConstraints::default()
        });
        let execution_metadata = Some(executor.binding_metadata());
        entry
            .executor
            .set_runtime_context(ServerSpawnRuntimeContext {
                parent_run_id: run_id.to_string(),
                user_id: user_id.to_string(),
                session_id: session_id.to_string(),
                trace_context: server_trace_context(user_id, session_id, run_id, turn_seq),
                forward_headers: request.forward_headers.clone(),
                admitted_model_execution: request.admitted_model_execution.clone(),
                request_constraints: request_constraints.clone(),
                execution_metadata: execution_metadata.clone(),
                spawner: Arc::downgrade(&entry.spawner),
                pause_flag,
                cancel_token,
                #[cfg(feature = "bridge-e2e-hooks")]
                test_child_llm_rounds: request
                    .context
                    .as_ref()
                    .and_then(|ctx| ctx.get("test_spawn_child_llm_rounds"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
                #[cfg(feature = "harness")]
                harness_sink,
            })
            .await;

        let agent_id = request
            .agent_id
            .clone()
            .unwrap_or_else(|| "root-agent".to_string());
        let active_work_registry = entry.active_work_registry.clone();
        executor.set_agent_tool_context(AgentToolContext {
            run_id: run_id.to_string(),
            agent_id,
            delegation_chain: Vec::new(),
            current_model: request.model.clone(),
            recursion_depth: 0,
            is_fork_child: false,
            working_dir: workspace.to_path_buf(),
            spawner: entry.spawner,
            inherited_permissions: Self::inherited_permissions_from_request(
                request,
                &request_constraints,
            ),
            active_skills: Vec::new(),
            live_event_sink: work_surface_event_tx
                .clone()
                .zip(work_surface_gap_tracker)
                .map(|(tx, gap_tracker)| {
                    Arc::new(WorkSurfaceAgentLiveEventSink::new(
                        tx,
                        execution_metadata.clone(),
                        gap_tracker,
                    )) as SharedAgentLiveEventSink
                }),
            client_tool_delivery_tx: work_surface_event_tx.clone(),
            trace_context: Some(server_trace_context(user_id, session_id, run_id, turn_seq)),
            execution_metadata,
            transcript_location: AgentTranscriptLocation::DurableServer,
        });
        active_work_registry
    }

    fn build_csl_store(
        &self,
        user_id: &str,
    ) -> Option<Arc<dyn astra_turn_core::conversation_log::CslStore>> {
        let pool = self.shared_pool.as_ref()?;
        let store = match astra_turn_core::conversation_log::db_store::DbCslStore::new(
            self.matrixone.clone(),
            user_id,
        ) {
            Ok(store) => store.with_pool(pool.clone()),
            Err(error) => {
                tracing::warn!(user_id, error = %error, "CSL store creation failed");
                return None;
            }
        };
        Some(Arc::new(store))
    }

    async fn restore_csl_history(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        loop_state: &mut AgenticLoopState,
    ) -> Option<astra_turn_core::conversation_log::manager::CslManager> {
        let store = self.build_csl_store(user_id)?;
        let mut mgr = match astra_turn_core::conversation_log::manager::CslManager::new(
            store,
            session_id.to_string(),
            Default::default(),
        ) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "CSL manager creation failed");
                return None;
            }
        };
        mgr.set_trace_id(run_id.to_string());

        let restored_messages;

        let mut csl_reusable = true;
        match mgr.load().await {
            Ok(Some(mat)) => {
                restored_messages = mat.messages;
                restore_session_state_compact(mat.session_state, loop_state);
            }
            Ok(None) => {
                restored_messages = self
                    .restore_transcript_prompt_messages(user_id, session_id, run_id, "csl_empty")
                    .await;
                if restored_messages.is_empty() {
                    self.record_runtime_retrieval_degrade(
                        user_id,
                        session_id,
                        run_id,
                        RetrievalStage::Structured,
                        "timeout",
                    )
                    .await;
                    self.record_runtime_retrieval_degrade(
                        user_id,
                        session_id,
                        run_id,
                        RetrievalStage::Fts,
                        "empty",
                    )
                    .await;
                    self.record_runtime_retrieval_degrade(
                        user_id,
                        session_id,
                        run_id,
                        RetrievalStage::Vector,
                        "stale",
                    )
                    .await;
                }
            }
            Err(e) => {
                tracing::warn!(
                    session_id,
                    error = %e,
                    "CSL load failed; starting with empty history"
                );
                self.record_runtime_retrieval_degrade(
                    user_id,
                    session_id,
                    run_id,
                    RetrievalStage::Structured,
                    "timeout",
                )
                .await;
                restored_messages = self
                    .restore_transcript_prompt_messages(
                        user_id,
                        session_id,
                        run_id,
                        "csl_load_failed",
                    )
                    .await;
                csl_reusable = matches!(
                    e,
                    astra_turn_core::conversation_log::CslStoreError::Serde(_)
                        | astra_turn_core::conversation_log::CslStoreError::Materialize(_)
                );
                if csl_reusable && let Err(reset_error) = mgr.reset().await {
                    tracing::warn!(
                        session_id,
                        run_id,
                        error = %reset_error,
                        "CSL reset failed after corrupted log; transcript fallback will not persist CSL this turn"
                    );
                    csl_reusable = false;
                }
            }
        }

        let turn_start_message_count =
            Self::restore_csl_messages_into_loop_state(restored_messages, loop_state);
        if !csl_reusable {
            return None;
        }
        mgr.mark_turn_start(turn_start_message_count);
        Some(mgr)
    }

    async fn restore_transcript_prompt_messages(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        reason: &'static str,
    ) -> Vec<Value> {
        let Some(pool) = self.shared_pool.as_ref() else {
            return Vec::new();
        };
        let rows = match sqlx::query(PROMPT_HISTORY_TRANSCRIPT_SELECT_SQL)
            .bind(session_id)
            .bind(user_id)
            .bind(astra_services::session_restore::MAX_PROMPT_HISTORY_TRANSCRIPT_ROWS)
            .fetch_all(pool.get())
            .await
        {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(
                    session_id,
                    run_id,
                    reason,
                    error = %error,
                    "transcript prompt-history restore failed"
                );
                return Vec::new();
            }
        };

        let mut messages = Vec::with_capacity(rows.len());
        for row in rows {
            let role = match row.try_get::<String, _>("role") {
                Ok(role) => role,
                Err(error) => {
                    tracing::warn!(
                        session_id,
                        run_id,
                        reason,
                        error = %error,
                        "transcript prompt-history restore skipped row with invalid role"
                    );
                    continue;
                }
            };
            if !matches!(role.as_str(), "user" | "assistant" | "system") {
                continue;
            }
            let content = match row.try_get::<String, _>("content") {
                Ok(content) => content,
                Err(error) => {
                    tracing::warn!(
                        session_id,
                        run_id,
                        reason,
                        error = %error,
                        "transcript prompt-history restore skipped row with invalid content"
                    );
                    continue;
                }
            };
            if content.trim().is_empty() {
                continue;
            }
            messages.push(json!({
                "role": role,
                "content": content,
            }));
        }

        let messages = astra_turn_core::prompt_facing::sanitize_prompt_facing_messages(messages);
        if !messages.is_empty() {
            tracing::warn!(
                session_id,
                run_id,
                reason,
                message_count = messages.len(),
                "restored prompt history from transcript because CSL was unavailable"
            );
        }
        messages
    }

    fn restore_csl_messages_into_loop_state(
        mut restored_messages: Vec<Value>,
        loop_state: &mut AgenticLoopState,
    ) -> usize {
        let turn_start_message_count = restored_messages.len();
        if !restored_messages.is_empty() {
            restored_messages.append(&mut loop_state.messages);
            loop_state.messages = restored_messages;
        }
        turn_start_message_count
    }

    async fn record_runtime_retrieval_degrade(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        stage: RetrievalStage,
        reason: &str,
    ) {
        let Some(pool) = &self.shared_pool else {
            return;
        };
        let store = DatabaseContextManifestStore::new(pool.clone());
        if let Err(error) = store
            .record_retrieval_degrade_event(
                user_id,
                session_id,
                Some(run_id),
                stage.clone(),
                reason,
                0,
            )
            .await
        {
            tracing::warn!(
                target: "astra_runtime::retrieval",
                session_id,
                run_id,
                stage = ?stage,
                reason,
                error = %error,
                "failed to persist retrieval degrade event"
            );
        }
    }

    /// Wait for all in-flight background agentic loop tasks to finish.
    ///
    /// Called during graceful shutdown. Polls the task counter with 100ms
    /// intervals up to `timeout`. Returns `true` if all tasks drained within
    /// the timeout, `false` if tasks are still running.
    async fn drain_background_tasks_impl(&self, timeout: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.background_task_count.load(Ordering::Acquire) == 0 {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                self.persist_graceful_shutdown_checkpoints().await;
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        // Turn-owned background_task_count reached zero, but the
        // session-memory extraction service has its own pending
        // counter (see `MemoryExtractionService::wait_for_pending`).
        // Fold it into the same shutdown deadline so we don't kill
        // in-flight Memoria writes mid-HTTP.
        if let Some(svc) = self.memory_extraction_service.as_ref() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let leftover = svc.wait_for_pending(remaining).await;
            if leftover > 0 {
                return false;
            }
        }
        true
    }

    async fn persist_graceful_shutdown_checkpoints(&self) {
        let engine = &self.run_engine;
        let runs_to_checkpoint = {
            let runs = self.runs.read().await;
            runs.values()
                .filter(|run| {
                    matches!(
                        run.status,
                        RunStatus::Running | RunStatus::Waiting | RunStatus::Paused
                    )
                })
                .map(|run| (run.user_id.clone(), run.run_id.clone()))
                .collect::<Vec<_>>()
        };
        for (user_id, run_id) in runs_to_checkpoint {
            let checkpoint = json!({
                "version": "checkpoint_v1",
                "graceful": true,
                "last_batch_id": format!("shutdown-{run_id}"),
                "extra": {}
            });
            astra_core::log_persist!(
                engine
                    .persist_checkpoint(&user_id, &run_id, &checkpoint.to_string())
                    .await,
                "run_lifecycle",
                &run_id,
                "graceful_shutdown_checkpoint"
            );
            astra_core::log_persist!(
                engine
                    .append_event(
                        &user_id,
                        &run_id,
                        json!({"event_type": "run_checkpointed_for_shutdown", "data": {}})
                    )
                    .await,
                "run_lifecycle",
                &run_id,
                "graceful_shutdown_checkpoint_event"
            );
        }
    }

    /// Returns the current number of in-flight background tasks.
    pub fn background_task_count(&self) -> usize {
        self.background_task_count.load(Ordering::Acquire)
    }

    /// Clone the Arc handle to the runs map (for background tasks).
    fn runs_handle(&self) -> Arc<RwLock<HashMap<String, RunState>>> {
        Arc::clone(&self.runs)
    }

    /// Schedule removal of a terminal run from the in-memory cache after a
    /// grace period. Clients have 5 minutes to poll final events before the
    /// entry is evicted. This prevents unbounded memory growth.
    fn schedule_run_eviction(runs: &Arc<RwLock<HashMap<String, RunState>>>, run_id: String) {
        let runs = Arc::clone(runs);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
        spawn_observed(
            async move {
                tokio::time::sleep_until(deadline).await;
                runs.write().await.remove(&run_id);
            },
            "run_eviction",
        );
    }

    fn build_tracked_run_state(
        run_id: String,
        session_id: String,
        user_id: String,
    ) -> (
        RunState,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
        Arc<CancellationToken>,
    ) {
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let pause_flag = Arc::new(AtomicBool::new(false));
        let llm_cancel_token = Arc::new(CancellationToken::new());
        let run_state = RunState {
            run_id,
            user_id,
            session_id,
            status: RunStatus::Running,
            events: vec![json!({"event_type": "run_started", "data": {}})],
            cancel_flag: cancel_flag.clone(),
            pause_flag: pause_flag.clone(),
            llm_cancel_token: llm_cancel_token.clone(),
            live_tx: None,
            waiting_for: None,
            execution_live: true,
        };
        (run_state, cancel_flag, pause_flag, llm_cancel_token)
    }

    /// Return true when an existing run should block starting a new turn in the
    /// same session.
    ///
    /// `paused` is intentionally split:
    /// - `paused + waiting_for=Some(..)` is a real user/approval wait and must
    ///   be resumed or cancelled before another run starts.
    /// - `paused + waiting_for=None` is used for resumable interruptions such as
    ///   `budget_exhausted`; the user-facing contract says the next message can
    ///   continue from the checkpoint, so it must not block a fresh web turn.
    fn blocks_new_session_run(run: &RunState, session_id: &str) -> bool {
        run.session_id == session_id && run.status.blocks_session(run.waiting_for.as_deref())
    }

    fn session_has_blocking_run(runs: &HashMap<String, RunState>, session_id: &str) -> bool {
        runs.values()
            .any(|run| Self::blocks_new_session_run(run, session_id))
    }

    async fn run_execution_is_live(&self, durable: &DurableRunRecord) -> bool {
        if let Some(run) = self.runs.read().await.get(&durable.run_id) {
            return run.execution_live;
        }
        durable_run_owner_lease_is_live(durable)
    }

    async fn reconcile_orphaned_execution_for_session_continuation(
        &self,
        durable: &DurableRunRecord,
        requested_operation: &'static str,
    ) -> Result<bool, (StatusCode, Json<ErrorResponse>)> {
        if !durable_run_status_blocks_session(&durable.status, durable.waiting_for.as_deref()) {
            return Ok(false);
        }
        let interruption_event = json!({
            "event_type": "run_interrupted",
            "data": {
                "kind": "executor_not_live",
                "previous_status": durable.status,
                "requested_operation": requested_operation,
                "resumable": true,
                "resume_strategy": "session_continuation",
                "releases_session_slot": true,
            }
        });
        let updated = self
            .run_engine
            .transition_status_with_event_if_current(
                &durable.user_id,
                &durable.run_id,
                &[durable.status.as_str()],
                STATUS_PAUSED,
                None,
                None,
                interruption_event.clone(),
            )
            .await
            .map_err(|error| Self::durable_persist_error("orphan recovery transition", error))?;
        if updated && let Some(run) = self.runs.write().await.get_mut(&durable.run_id) {
            run.status = RunStatus::Paused;
            run.waiting_for = None;
            run.execution_live = false;
            run.pause_flag.store(false, Ordering::SeqCst);
            run.live_tx = None;
            run.events.push(interruption_event);
        }
        Ok(updated)
    }

    fn configure_loop_state_runtime_controls(
        &self,
        loop_state: &mut AgenticLoopState,
        cancel_flag: &Arc<AtomicBool>,
        pause_flag: &Arc<AtomicBool>,
        llm_cancel_token: CancellationToken,
    ) {
        loop_state.cancellation.flag = Some(cancel_flag.clone());
        loop_state.cancellation.pause_flag = Some(pause_flag.clone());
        loop_state.cancellation.token = Some(Arc::new(llm_cancel_token));
        loop_state.delegation_engine = self.delegation_engine.clone();
        // Wire cross-pod cancel/pause provider so the agentic loop can poll
        // DB for control signals from other pods in horizontally-scaled deployments.
        loop_state.run_control = Some(Arc::new(self.run_engine.clone()));
    }

    async fn persist_run_start(
        &self,
        run_id: &str,
        user_id: &str,
        session_id: &str,
        request: &ChatRequestData,
        execution_bindings: Option<&ExecutionBindingSnapshot>,
        agent_binding_context: Option<&PreparedAgentBindingLoopContext>,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        self.run_engine
            .start_run_with_context(
                run_id,
                user_id,
                session_id,
                run_start_context_from_request(
                    request,
                    execution_bindings,
                    agent_binding_context.map(|context| &context.binding),
                ),
            )
            .await
            .map_err(|error| {
                let status = if error == "session already has an active run" {
                    StatusCode::CONFLICT
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                };
                let detail = if status == StatusCode::CONFLICT {
                    error
                } else {
                    format!("Failed to persist durable run start: {error}")
                };
                error_response(status, detail)
            })
    }

    async fn fail_started_run_before_spawn(
        &self,
        user_id: &str,
        run_id: &str,
        message: &str,
        failure_code: PreSpawnFailureCode,
    ) {
        let terminal_events = pre_spawn_failure_terminal_events(message, failure_code);
        astra_core::log_persist!(
            self.run_engine
                .transition_status_with_events_if_current(
                    user_id,
                    run_id,
                    &[STATUS_RUNNING, STATUS_PAUSED, STATUS_WAITING,],
                    STATUS_FAILED,
                    None,
                    Some(message),
                    &terminal_events,
                )
                .await,
            "run_lifecycle",
            run_id,
            "pre_spawn_failure_transition"
        );
        self.runs.write().await.remove(run_id);
    }

    fn finalize_run_events(
        loop_outcome: Result<AgenticLoopOutcome, astra_core::ClassifiedError>,
        mut events: Vec<Value>,
        loop_state: &AgenticLoopState,
    ) -> (Vec<Value>, RunStatus, Option<String>) {
        let total_input =
            loop_state.total_prompt + loop_state.total_cache_read + loop_state.total_cache_creation;
        let usage = json!({
            "prompt_tokens": total_input,
            "completion_tokens": loop_state.total_completion,
            "tool_call_count": loop_state.total_tool_calls,
        });
        let cancellation_requested = loop_state
            .cancellation
            .flag
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Acquire))
            || loop_state
                .cancellation
                .token
                .as_ref()
                .is_some_and(|t| t.is_cancelled());

        let (final_status, error_msg) = if cancellation_requested
            || matches!(&loop_outcome, Ok(AgenticLoopOutcome::Cancelled))
        {
            let mut data = usage;
            data["cancelled"] = Value::Bool(true);
            events.push(json!({
                "event_type": "run_finished",
                "data": data,
            }));
            (RunStatus::Cancelled, None)
        } else {
            match loop_outcome {
                Ok(AgenticLoopOutcome::Delegated) => {
                    let mut finished = usage;
                    finished["status"] = Value::String(STATUS_DELEGATED.to_string());
                    finished["outcome"] = Value::String(STATUS_DELEGATED.to_string());
                    events.push(json!({
                        "event_type": "run_finished",
                        "data": finished,
                    }));
                    (RunStatus::Delegated, None)
                }
                Ok(AgenticLoopOutcome::ControlRejected(rejection)) => {
                    events.push(json!({
                        "event_type": "run_error",
                        "data": {
                            "error": rejection.message.clone(),
                            "error_code": rejection.code,
                            "error_kind": "contract_violation",
                        }
                    }));
                    let mut finished = usage;
                    finished["error"] = Value::String(rejection.message.clone());
                    finished["error_code"] = Value::String(rejection.code.to_string());
                    finished["error_kind"] = Value::String("contract_violation".to_string());
                    events.push(json!({
                        "event_type": "run_finished",
                        "data": finished,
                    }));
                    (RunStatus::Failed, Some(rejection.message))
                }
                Ok(AgenticLoopOutcome::Completed) => {
                    if let Some(interruption) = loop_state.interruption.as_ref() {
                        let task_board_snapshot = loop_state.hooks.task_board_snapshot.clone();
                        let waiting_for = if matches!(
                            interruption.resume_action,
                            astra_turn_core::interruption::ResumeAction::RequiresIntervention { .. }
                                | astra_turn_core::interruption::ResumeAction::StartNewSession
                        ) {
                            Some("user_intervention".to_string())
                        } else {
                            None
                        };
                        let task_board_payload =
                            task_board_settlement_payload(&task_board_snapshot);
                        let mut interruption_json = interruption.to_json();
                        if let Some(obj) = interruption_json.as_object_mut() {
                            if let Some(waiting_for) = waiting_for.as_ref() {
                                obj.insert(
                                    "waiting_for".to_string(),
                                    Value::String(waiting_for.clone()),
                                );
                            }
                            if let Some(task_board) = task_board_payload {
                                obj.insert("task_board".to_string(), task_board);
                            }
                        }
                        if !loop_state.final_text.is_empty() {
                            events.push(json!({
                                "event_type": "text_done",
                                "data": {
                                    "full_text": loop_state.final_text.clone(),
                                    "partial": true,
                                    "interruption": interruption_json.clone(),
                                }
                            }));
                        }
                        events.push(json!({
                            "event_type": "run_interrupted",
                            "data": interruption_json.clone(),
                        }));
                        let mut finished = usage;
                        finished["interrupted"] = Value::Bool(true);
                        finished["interruption_kind"] =
                            Value::String(interruption.kind.label().to_string());
                        finished["resumable"] = Value::Bool(interruption.kind.is_resumable());
                        if let Some(waiting_for) = waiting_for.as_ref() {
                            finished["waiting_for"] = Value::String(waiting_for.clone());
                        }
                        events.push(json!({
                            "event_type": "run_finished",
                            "data": finished,
                        }));
                        (RunStatus::Paused, waiting_for)
                    } else {
                        if !loop_state.final_text.is_empty() {
                            events.push(json!({
                            "event_type": "text_done",
                            "data": { "full_text": loop_state.final_text.clone() }
                            }));
                        }
                        let mut finished = usage;
                        if let Some(task_board) =
                            task_board_settlement_payload(&loop_state.hooks.task_board_snapshot)
                        {
                            finished["task_board"] = task_board;
                        }
                        events.push(json!({
                            "event_type": "run_finished",
                            "data": finished,
                        }));
                        (RunStatus::Completed, None)
                    }
                }
                Ok(AgenticLoopOutcome::Cancelled) => {
                    // This branch should be handled by the cancellation gate check above,
                    // but handle it gracefully instead of panicking in production.
                    let mut data = usage;
                    data["cancelled"] = Value::Bool(true);
                    events.push(json!({
                        "event_type": "run_finished",
                        "data": data,
                    }));
                    (RunStatus::Cancelled, None)
                }
                Ok(AgenticLoopOutcome::Error(e)) => {
                    let classified = astra_core::ClassifiedError::from(e.clone());
                    let error_kind = classified.kind.as_str();
                    events.push(json!({
                        "event_type": "run_error",
                        "data": {
                            "error": e.clone(),
                            "error_code": error_kind,
                            "error_kind": error_kind,
                        }
                    }));
                    let mut finished = usage.clone();
                    finished["error"] = Value::String(e.clone());
                    finished["error_code"] = Value::String(error_kind.to_string());
                    finished["error_kind"] = Value::String(error_kind.to_string());
                    events.push(json!({
                        "event_type": "run_finished",
                        "data": finished,
                    }));
                    (RunStatus::Failed, Some(e))
                }
                Ok(AgenticLoopOutcome::Waiting(w)) => {
                    let msg = format!("waiting: {w}");
                    events.push(json!({
                        "event_type": "run_waiting",
                        "data": {
                            "reason": msg,
                            "resumable": true,
                            "resume_strategy": "session_continuation",
                            "execution_live": false,
                        }
                    }));
                    events.push(json!({
                        "event_type": "run_finished",
                        "data": {
                            "interrupted": true,
                            "resumable": true,
                            "resume_strategy": "session_continuation",
                        }
                    }));
                    (RunStatus::Paused, None)
                }
                Err(err) => {
                    let msg = err.to_string();
                    let error_kind = err.kind.as_str();
                    let error_code = classified_terminal_error_code(&err);
                    events.push(json!({
                        "event_type": "run_error",
                        "data": {
                            "error": &msg,
                            "error_code": &error_code,
                            "error_kind": error_kind,
                        }
                    }));
                    let mut finished = usage;
                    finished["error"] = Value::String(msg.clone());
                    finished["error_code"] = Value::String(error_code);
                    finished["error_kind"] = Value::String(error_kind.to_string());
                    events.push(json!({
                        "event_type": "run_finished",
                        "data": finished,
                    }));
                    (RunStatus::Failed, Some(msg))
                }
            }
        };

        (events, final_status, error_msg)
    }

    /// Validate the request and return the parsed [`RequestConstraints`].
    ///
    /// The returned constraints are the ones every downstream consumer
    /// (`build_initial_state`, dynamic-agent spawner wiring, delegation
    /// engine) must use — re-parsing wire shape after this point is the bug
    /// pattern that motivated the refactor. Callers that just need
    /// validation and don't take the constraints can drop the result with
    /// `let _ = ...?;`.
    async fn validate_request_constraints(
        &self,
        user_id: &str,
        request: &ChatRequestData,
    ) -> Result<RequestConstraints, (StatusCode, Json<ErrorResponse>)> {
        Self::validate_resolved_model_selection(request)?;
        self.validate_runtime_profile_shape(request)?;
        Self::validate_runtime_auth_shape(request)?;
        let provider_model_descriptor = Self::provider_model_descriptor(request)?;
        if provider_model_descriptor.is_some() {
            Self::validate_provider_runtime_authorized(request)?;
        }
        if request.capability_descriptors.is_some() && !request.provider_runtime_authorized {
            return Err(error_response_coded(
                StatusCode::BAD_REQUEST,
                "capability_descriptors require provider-authorized request authentication",
                "provider_runtime_context_required",
            ));
        }
        if request
            .mcp_binding_ids
            .as_deref()
            .is_some_and(|ids| !ids.is_empty())
        {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "mcp_binding_ids is no longer supported on /chat/stream; use runtime_mcp_bindings"
                    .to_string(),
            ));
        }
        let request_constraints = Self::try_request_constraints(request)
            .map_err(|detail| error_response(StatusCode::BAD_REQUEST, detail))?;
        if let Some(enabled_tools) = request_constraints.enabled_tools.as_ref() {
            let fallback_registry = astra_runtime_env::ToolRegistry::builtins();
            let registry = self
                .tool_execution_service
                .as_ref()
                .map(ToolExecutionService::tool_registry)
                .unwrap_or(&fallback_registry);
            for tool_name in enabled_tools {
                let Some(spec) = registry.get(tool_name) else {
                    return Err(error_response_coded(
                        StatusCode::BAD_REQUEST,
                        format!("enabled_tools contains unknown tool '{tool_name}'"),
                        "enabled_tools_invalid",
                    ));
                };
                if !spec.requires_explicit_user_enablement() {
                    return Err(error_response_coded(
                        StatusCode::BAD_REQUEST,
                        format!(
                            "enabled_tools may contain only product-optional external tools; '{tool_name}' is a core runtime tool"
                        ),
                        "enabled_tools_invalid",
                    ));
                }
            }
        }
        if request.agent_binding.is_none() && request.runtime_skill_binding.is_none() {
            let (_, resolver) = build_server_skill_resolver(self.skill_service.clone(), user_id);
            apply_normalized_skill_allowlist(resolver, &request_constraints)
                .map_err(|detail| error_response(StatusCode::BAD_REQUEST, detail))?;
        }
        Ok(request_constraints)
    }

    async fn validate_optional_tool_availability(
        &self,
        user_id: &str,
        constraints: &RequestConstraints,
        execution_bindings: Option<&ExecutionBindingSnapshot>,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let Some(enabled_tools) = constraints
            .enabled_tools
            .as_ref()
            .filter(|tools| !tools.is_empty())
        else {
            return Ok(());
        };
        let Some(service) = self.tool_execution_service.as_ref() else {
            return Err(error_response_coded(
                StatusCode::CONFLICT,
                "optional tools were enabled, but this runtime has no tool execution provider"
                    .to_string(),
                "optional_tool_provider_unavailable",
            ));
        };
        let unavailable = service
            .unavailable_optional_tools_for_binding(user_id, enabled_tools, execution_bindings)
            .await;
        if unavailable.is_empty() {
            return Ok(());
        }
        let selected_provider = execution_bindings
            .filter(|snapshot| snapshot.executor.kind == ExecutorBindingKind::EdgeAgent)
            .map(|snapshot| format!("bound edge '{}'", snapshot.executor.executor_id))
            .unwrap_or_else(|| "server deployment".to_string());
        Err(error_response_coded(
            StatusCode::CONFLICT,
            format!(
                "{selected_provider} does not currently provide the enabled optional tools: {}",
                unavailable.join(", ")
            ),
            "optional_tool_provider_unavailable",
        ))
    }

    async fn prepare_chat_request(
        &self,
        mut request: ChatRequestData,
    ) -> Result<ChatRequestData, (StatusCode, Json<ErrorResponse>)> {
        Self::validate_effective_user_input(&request)?;
        let selection = Self::validate_model_selection_shape(request.model_selection.as_ref())?;
        if request.admitted_model_execution.is_some() {
            return Err(error_response_coded(
                StatusCode::BAD_REQUEST,
                "admitted_model_execution is trusted runtime context and cannot be supplied by clients",
                "model_selection_invalid",
            ));
        }
        if request.provider_runtime_authorized {
            let resolved = request.resolved_model_selection.as_ref().ok_or_else(|| {
                error_response_coded(
                    StatusCode::BAD_REQUEST,
                    "provider-authorized model selection is missing its trusted resolution",
                    "provider_runtime_context_required",
                )
            })?;
            if resolved.offering_id != selection.offering_id {
                return Err(error_response_coded(
                    StatusCode::FORBIDDEN,
                    "provider-resolved model selection does not match the requested Offering",
                    "model_offering_mismatch",
                ));
            }
            exact_runtime_string(
                "resolved_model_selection.model_name",
                &resolved.model_name,
                "provider_runtime_context_invalid",
            )?;
            let gateway = Self::provider_model_descriptor(&request)?.ok_or_else(|| {
                error_response_coded(
                    StatusCode::BAD_REQUEST,
                    "provider-authorized model execution requires capability_descriptors.model_gateway",
                    "provider_runtime_context_required",
                )
            })?;
            request.admitted_model_execution = Some(
                crate::server::model_execution_admission::admit_model_execution(
                    &self.model_service,
                    selection,
                    Some(resolved),
                    Some(gateway),
                    request.runtime_auth.as_ref(),
                )
                .await?,
            );
            request.model = Some(resolved.model_name.clone());
            return Ok(request);
        }
        if request.resolved_model_selection.is_some() {
            return Err(error_response_coded(
                StatusCode::BAD_REQUEST,
                "resolved_model_selection is trusted Server context and cannot be supplied by clients",
                "model_selection_invalid",
            ));
        }
        let admitted = crate::server::model_execution_admission::admit_model_execution(
            &self.model_service,
            selection,
            None,
            None,
            None,
        )
        .await?;
        let resolved = ResolvedModelSelection {
            offering_id: admitted.offering_id.clone(),
            model_name: admitted.model_name.clone(),
        };
        request.model = Some(resolved.model_name.clone());
        request.resolved_model_selection = Some(resolved);
        request.admitted_model_execution = Some(admitted);
        Ok(request)
    }

    fn validate_effective_user_input(
        request: &ChatRequestData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        if !request.message.trim().is_empty() {
            return Ok(());
        }
        if request
            .user_intent
            .as_deref()
            .is_some_and(|intent| !intent.trim().is_empty())
        {
            return Ok(());
        }
        Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "message or user_intent must contain the user's current request",
            "chat_input_empty",
        ))
    }

    fn validate_model_selection_shape(
        model_selection: Option<&ModelSelectionRequest>,
    ) -> Result<&ModelSelectionRequest, (StatusCode, Json<ErrorResponse>)> {
        let model_selection = model_selection.ok_or_else(|| {
            error_response_coded(
                StatusCode::BAD_REQUEST,
                "model_selection.offering_id is required",
                "model_selection_missing",
            )
        })?;
        astra_services::validate_model_offering_id(&model_selection.offering_id).map_err(|_| {
            error_response_coded(
                StatusCode::BAD_REQUEST,
                "model_selection.offering_id must be an exact non-empty identifier of at most 64 bytes",
                "model_selection_invalid",
            )
        })?;
        Ok(model_selection)
    }

    fn validate_resolved_model_selection(
        request: &ChatRequestData,
    ) -> Result<&ResolvedModelSelection, (StatusCode, Json<ErrorResponse>)> {
        let selection = Self::validate_model_selection_shape(request.model_selection.as_ref())?;
        let resolved = request.resolved_model_selection.as_ref().ok_or_else(|| {
            error_response_coded(
                StatusCode::INTERNAL_SERVER_ERROR,
                "model selection reached execution without Server resolution",
                "model_resolution_missing",
            )
        })?;
        if resolved.offering_id != selection.offering_id
            || request.model.as_deref() != Some(resolved.model_name.as_str())
        {
            return Err(error_response_coded(
                StatusCode::INTERNAL_SERVER_ERROR,
                "resolved model identity is inconsistent with the admitted Offering",
                "model_resolution_inconsistent",
            ));
        }
        let execution = request.admitted_model_execution.as_ref().ok_or_else(|| {
            error_response_coded(
                StatusCode::INTERNAL_SERVER_ERROR,
                "model selection reached execution without admitted execution material",
                "model_material_missing",
            )
        })?;
        if execution.offering_id != resolved.offering_id
            || execution.model_name != resolved.model_name
        {
            return Err(error_response_coded(
                StatusCode::INTERNAL_SERVER_ERROR,
                "model execution material is inconsistent with the admitted Offering",
                "model_resolution_inconsistent",
            ));
        }
        Ok(resolved)
    }

    fn validate_runtime_profile_shape(
        &self,
        request: &ChatRequestData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        if request.agent_binding.is_some() {
            Self::validate_agent_binding_context_shape(request)?;
            if !request.runtime_mcp_bindings.is_empty() {
                return Err(error_response_coded(
                    StatusCode::BAD_REQUEST,
                    "agent_binding cannot be combined with runtime_mcp_bindings",
                    "agent_binding_runtime_profile_conflict",
                ));
            }
            if request.runtime_skill_binding.is_some() {
                return Err(error_response_coded(
                    StatusCode::BAD_REQUEST,
                    "agent_binding cannot be combined with runtime_skill_binding",
                    "agent_binding_runtime_profile_conflict",
                ));
            }
            if request
                .mcp_binding_ids
                .as_deref()
                .is_some_and(|ids| !ids.is_empty())
            {
                return Err(error_response_coded(
                    StatusCode::BAD_REQUEST,
                    "agent_binding cannot be combined with mcp_binding_ids",
                    "agent_binding_runtime_profile_conflict",
                ));
            }
            if matches!(
                request.runtime_profile,
                Some(RuntimeProfileRequest::RequestScopedRuntimeMcp)
            ) {
                return Err(error_response_coded(
                    StatusCode::BAD_REQUEST,
                    "agent_binding requires agent_binding_registry runtime profile",
                    "agent_binding_runtime_profile_conflict",
                ));
            }
        } else if matches!(
            request.runtime_profile,
            Some(RuntimeProfileRequest::AgentBindingRegistry)
        ) {
            return Err(error_response_coded(
                StatusCode::BAD_REQUEST,
                "runtime_profile=agent_binding_registry requires agent_binding",
                "agent_binding_runtime_profile_conflict",
            ));
        } else if !request.runtime_mcp_bindings.is_empty()
            && !matches!(
                request.runtime_profile,
                Some(RuntimeProfileRequest::RequestScopedRuntimeMcp)
            )
        {
            return Err(error_response_coded(
                StatusCode::BAD_REQUEST,
                "runtime_mcp_bindings requires runtime_profile=request_scoped_runtime_mcp",
                "agent_binding_runtime_profile_conflict",
            ));
        }
        Ok(())
    }

    fn validate_agent_binding_context_shape(
        request: &ChatRequestData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let Some(context) = request.context.as_ref() else {
            return Ok(());
        };
        Self::reject_agent_binding_context_array(
            context,
            "edge_tools",
            "Agent Binding mode cannot carry request-scoped edge_tools",
        )?;
        Self::reject_agent_binding_context_array(
            context,
            "edge_skills",
            "Agent Binding mode cannot carry request-scoped edge_skills",
        )?;
        Ok(())
    }

    fn reject_agent_binding_context_array(
        context: &Map<String, Value>,
        field: &'static str,
        detail: &'static str,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let Some(value) = context.get(field) else {
            return Ok(());
        };
        if value.as_array().is_some_and(Vec::is_empty) {
            return Ok(());
        }
        Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            detail,
            "agent_binding_runtime_profile_conflict",
        ))
    }

    fn validate_runtime_auth_shape(
        request: &ChatRequestData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let required = request.agent_binding.is_some()
            || request
                .capability_descriptors
                .as_ref()
                .and_then(|descriptors| descriptors.model_gateway.as_ref())
                .is_some();
        let Some(runtime_auth) = request.runtime_auth.as_ref() else {
            if required {
                return Err(error_response_coded(
                    StatusCode::BAD_REQUEST,
                    "runtime_auth.authorization is required for the resolved provider runtime",
                    "agent_binding_runtime_auth_missing",
                ));
            }
            return Ok(());
        };
        validate_runtime_authorization(runtime_auth)
    }

    fn provider_model_descriptor(
        request: &ChatRequestData,
    ) -> Result<
        Option<&astra_services::runs::RuntimeCapabilityDescriptorRequest>,
        (StatusCode, Json<ErrorResponse>),
    > {
        let Some(descriptors) = request.capability_descriptors.as_ref() else {
            return Ok(None);
        };
        let Some(model_gateway) = descriptors.model_gateway.as_ref() else {
            return Err(error_response_coded(
                StatusCode::BAD_REQUEST,
                "capability_descriptors.model_gateway is required",
                "provider_runtime_context_required",
            ));
        };
        astra_services::auth::provider_request::validate_runtime_capability_descriptor(
            model_gateway,
            "model_gateway",
        )?;
        Ok(Some(model_gateway))
    }

    fn validate_provider_runtime_authorized(
        request: &ChatRequestData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        if !request.provider_runtime_authorized {
            return Err(error_response_coded(
                StatusCode::BAD_REQUEST,
                "provider runtime descriptors require provider-authorized request authentication",
                "provider_runtime_context_required",
            ));
        }
        Ok(())
    }

    async fn resolve_agent_binding_runtime(
        &self,
        request: &AgentBindingRuntimeRequest,
    ) -> Result<ResolvedAgentBindingRuntime, (StatusCode, Json<ErrorResponse>)> {
        exact_runtime_id(
            "agent_binding.capability_server_refs.mcp",
            &request.capability_server_refs.mcp,
        )?;
        exact_runtime_id(
            "agent_binding.capability_server_refs.skills",
            &request.capability_server_refs.skills,
        )?;
        let binding = self
            .agent_binding_service
            .get_binding(request.id.clone())
            .await?;
        match binding.status {
            astra_services::AgentBindingStatus::Active => {}
            astra_services::AgentBindingStatus::Disabled
            | astra_services::AgentBindingStatus::Invalid => {
                return Err(error_response_coded(
                    StatusCode::CONFLICT,
                    "agent binding is disabled for new turns",
                    "agent_binding_disabled",
                ));
            }
        }

        let mcp = binding
            .capability_servers
            .iter()
            .find(|server| server.id == request.capability_server_refs.mcp)
            .cloned()
            .ok_or_else(|| {
                error_response_coded(
                    StatusCode::BAD_REQUEST,
                    "agent_binding.capability_server_refs.mcp does not exist in binding",
                    "agent_binding_capability_ref_missing",
                )
            })?;
        if mcp.server_type != astra_services::CapabilityServerType::Mcp {
            return Err(error_response_coded(
                StatusCode::BAD_REQUEST,
                "agent_binding.capability_server_refs.mcp does not reference an mcp server",
                "agent_binding_capability_ref_invalid",
            ));
        }

        let skills = binding
            .capability_servers
            .iter()
            .find(|server| server.id == request.capability_server_refs.skills)
            .cloned()
            .ok_or_else(|| {
                error_response_coded(
                    StatusCode::BAD_REQUEST,
                    "agent_binding.capability_server_refs.skills does not exist in binding",
                    "agent_binding_capability_ref_missing",
                )
            })?;
        if skills.server_type != astra_services::CapabilityServerType::Skill {
            return Err(error_response_coded(
                StatusCode::BAD_REQUEST,
                "agent_binding.capability_server_refs.skills does not reference a skill server",
                "agent_binding_capability_ref_invalid",
            ));
        }

        Ok(ResolvedAgentBindingRuntime {
            binding,
            mcp_server: mcp,
            skill_server: skills,
        })
    }

    fn agent_binding_runtime_descriptor<'a>(
        label: &'static str,
        descriptor: Option<&'a astra_services::runs::RuntimeCapabilityDescriptorRequest>,
        expected_id: &str,
        expected_type: &str,
    ) -> Result<
        &'a astra_services::runs::RuntimeCapabilityDescriptorRequest,
        (StatusCode, Json<ErrorResponse>),
    > {
        let descriptor = descriptor.ok_or_else(|| {
            error_response_coded(
                StatusCode::BAD_REQUEST,
                format!("{label} is required when agent_binding is present"),
                "provider_runtime_context_required",
            )
        })?;
        astra_services::auth::provider_request::validate_runtime_capability_descriptor(
            descriptor,
            expected_type,
        )?;
        if descriptor.id != expected_id {
            return Err(error_response_coded(
                StatusCode::BAD_REQUEST,
                format!("{label}.id must match agent_binding capability server ref"),
                "agent_binding_capability_ref_invalid",
            ));
        }
        Ok(descriptor)
    }

    async fn prepare_runtime_capabilities(
        &self,
        request: &ChatRequestData,
        request_constraints: &RequestConstraints,
    ) -> Result<PreparedRuntimeCapabilities, (StatusCode, Json<ErrorResponse>)> {
        let Some(agent_binding) = request.agent_binding.as_ref() else {
            let mcp_bundle =
                runtime_mcp::prepare_request_scoped_runtime_bundle(&request.runtime_mcp_bindings)
                    .await?;
            let request_scoped_skill_resolver =
                if let Some(skill_binding) = request.runtime_skill_binding.as_ref() {
                    let resolver = agent_binding_skill_runtime::prepare_runtime_skill_resolver(
                        &skill_binding.id,
                        &skill_binding.url,
                        &skill_binding.authorization,
                    )
                    .await?;
                    apply_normalized_skill_allowlist(resolver, request_constraints)
                        .map_err(|detail| error_response(StatusCode::BAD_REQUEST, detail))?
                } else {
                    None
                };
            return Ok(PreparedRuntimeCapabilities {
                mcp_bundle,
                request_scoped_skill_resolver,
                agent_binding: None,
            });
        };
        let resolved = self.resolve_agent_binding_runtime(agent_binding).await?;
        let runtime_auth = request.runtime_auth.as_ref().ok_or_else(|| {
            error_response_coded(
                StatusCode::BAD_REQUEST,
                "runtime_auth.authorization is required when agent_binding is present",
                "agent_binding_runtime_auth_missing",
            )
        })?;
        let descriptors = request.capability_descriptors.as_ref().ok_or_else(|| {
            error_response_coded(
                StatusCode::BAD_REQUEST,
                "capability_descriptors is required when agent_binding is present",
                "provider_runtime_context_required",
            )
        })?;
        let mcp_descriptor = Self::agent_binding_runtime_descriptor(
            "capability_descriptors.mcp",
            descriptors.mcp.as_ref(),
            &resolved.mcp_server.id,
            "mcp",
        )?;
        let mcp_endpoint_url = mcp_descriptor.endpoint_url.clone();
        let skill_endpoint_url = Self::agent_binding_runtime_descriptor(
            "capability_descriptors.skills",
            descriptors.skills.as_ref(),
            &resolved.skill_server.id,
            "skills",
        )?
        .endpoint_url
        .clone();
        tracing::debug!(
            binding_id = %resolved.binding.id,
            binding_name = %resolved.binding.binding_name,
            mcp_server_id = %resolved.mcp_server.id,
            skill_server_id = %resolved.skill_server.id,
            "resolved Agent Binding capability servers"
        );
        let bundle = runtime_mcp::prepare_agent_binding_mcp_bundle(
            &resolved.mcp_server.id,
            &mcp_endpoint_url,
            &runtime_auth.authorization,
            mcp_descriptor.semantic_read.as_ref(),
        )
        .await?;
        let skill_resolver = agent_binding_skill_runtime::prepare_agent_binding_skill_resolver(
            &resolved.skill_server.id,
            &skill_endpoint_url,
            &runtime_auth.authorization,
        )
        .await?;
        let skill_resolver = apply_normalized_skill_allowlist(skill_resolver, request_constraints)
            .map_err(|detail| error_response(StatusCode::BAD_REQUEST, detail))?;
        Ok(PreparedRuntimeCapabilities {
            mcp_bundle: Some(bundle),
            request_scoped_skill_resolver: None,
            agent_binding: Some(PreparedAgentBindingLoopContext {
                binding: resolved.binding,
                skill_resolver,
            }),
        })
    }

    fn runtime_profile_manifest_label(request: &ChatRequestData) -> &'static str {
        if request.agent_binding.is_some() {
            "agent_binding_registry"
        } else if !request.runtime_mcp_bindings.is_empty()
            || request.runtime_skill_binding.is_some()
            || matches!(
                request.runtime_profile,
                Some(RuntimeProfileRequest::RequestScopedRuntimeMcp)
            )
        {
            "request_scoped_runtime_mcp"
        } else {
            "astra_native"
        }
    }

    fn discovered_skill_manifest_from_resolver(
        skill_resolver: Option<&Arc<dyn crate::turn::skill_tool::SkillResolver>>,
    ) -> Vec<Value> {
        skill_resolver
            .map(|resolver| {
                resolver
                    .available_skills()
                    .into_iter()
                    .map(|skill| {
                        json!({
                            "name": skill.name,
                            "description": skill.description,
                            "when_to_use": skill.when_to_use,
                            "aliases": skill.aliases,
                            "category": skill.category,
                            "tags": skill.tags,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn discovered_skill_manifest(
        agent_binding_context: Option<&PreparedAgentBindingLoopContext>,
    ) -> Vec<Value> {
        Self::discovered_skill_manifest_from_resolver(
            agent_binding_context.and_then(|context| context.skill_resolver.as_ref()),
        )
    }

    fn build_runtime_manifest(
        request: &ChatRequestData,
        runtime_capabilities: &PreparedRuntimeCapabilities,
        workspace_executor_admitted: bool,
    ) -> Option<Value> {
        let model_selection = request.model_selection.as_ref()?;
        let resolved_model = request.resolved_model_selection.as_ref()?;
        let model_resolution = if let Some(model_gateway) = request
            .capability_descriptors
            .as_ref()
            .and_then(|descriptors| descriptors.model_gateway.as_ref())
        {
            json!({
                "source": "provider_descriptor",
                "descriptor": {
                    "id": &model_gateway.id,
                    "protocol": &model_gateway.protocol,
                    "invoke_url_present": true
                }
            })
        } else {
            json!({
                "source": "catalog_offering",
                "offering_id": &resolved_model.offering_id,
                "model": &resolved_model.model_name,
                "resolved": true
            })
        };
        let turn_context = request
            .context
            .as_ref()
            .map(|context| Value::Object(context.clone()))
            .unwrap_or(Value::Null);
        let mut manifest = json!({
            "schema_version": "astra_runtime_manifest.v1",
            "model_selection": {
                "offering_id": &model_selection.offering_id,
            },
            "model_resolution": model_resolution,
            "runtime_profile": Self::runtime_profile_manifest_label(request),
            "turn": {
                "message": &request.message,
                "parts": &request.parts,
                "attachments": &request.attachments,
                "edge_executor_id": &request.edge_executor_id,
                "capabilities": &request.capabilities,
                "requested_capabilities": &request.capabilities,
                "context": turn_context
            },
            "capacity_resolution": {
                "requested_capabilities": &request.capabilities,
                "workspace_executor_admitted": workspace_executor_admitted,
                "server_builtin_surface": "server_service_control_plane_only"
            }
        });

        if let Some(bundle) = runtime_capabilities.mcp_bundle.as_ref() {
            manifest["provider_snapshot_refs"] = Value::Array(
                bundle
                    .provider_snapshots
                    .iter()
                    .map(|snapshot| {
                        json!({
                            "provider_identity": snapshot.provider_identity.as_str(),
                            "binding_ref": snapshot.binding_ref.as_str(),
                            "protocol": snapshot.protocol.as_str(),
                            "content_hash": &snapshot.content_hash,
                            "discovery_content_hash": &snapshot.discovery_snapshot_hash,
                            "resolver_version": snapshot.resolver_version.as_str(),
                            "tool_count": snapshot.descriptors.len(),
                        })
                    })
                    .collect(),
            );
        }

        if let (Some(binding_request), Some(binding_context)) = (
            request.agent_binding.as_ref(),
            runtime_capabilities.agent_binding.as_ref(),
        ) {
            let discovered_tools = runtime_capabilities
                .mcp_bundle
                .as_ref()
                .map(|bundle| bundle.schemas.clone())
                .unwrap_or_default();
            let discovered_skills = Self::discovered_skill_manifest(Some(binding_context));
            manifest["agent_binding"] = json!({
                "id": &binding_context.binding.id,
                "binding_name": &binding_context.binding.binding_name,
                "binding_schema_version": &binding_context.binding.binding_schema_version,
                "agent_md": &binding_context.binding.agent_md,
                "runtime_policy": &binding_context.binding.runtime_policy,
                "selected_capability_server_refs": {
                    "mcp": &binding_request.capability_server_refs.mcp,
                    "skills": &binding_request.capability_server_refs.skills
                },
                "discovered_tools": discovered_tools,
                "discovered_skills": discovered_skills,
            });
        }
        if let Some(skill_resolver) = runtime_capabilities.request_scoped_skill_resolver.as_ref() {
            manifest["request_scoped_runtime"] = json!({
                "discovered_skills": Self::discovered_skill_manifest_from_resolver(Some(skill_resolver)),
            });
        }

        Some(manifest)
    }

    fn install_agent_binding_runtime_forward_headers(
        request: &mut ChatRequestData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        if request.agent_binding.is_none() {
            return Ok(());
        }
        let runtime_auth = request.runtime_auth.as_ref().ok_or_else(|| {
            error_response_coded(
                StatusCode::BAD_REQUEST,
                "runtime_auth.authorization is required when agent_binding is present",
                "agent_binding_runtime_auth_missing",
            )
        })?;
        request.forward_headers.insert(
            "authorization".to_string(),
            runtime_auth.authorization.clone(),
        );
        Ok(())
    }

    fn agent_binding_prompt_section(binding: &astra_services::AgentBindingRecord) -> String {
        format!("## Agent Binding Instruction\n{}", binding.agent_md)
    }

    fn prompt_visible_context_key_tokens(key: &str) -> Vec<String> {
        let chars = key.chars().collect::<Vec<_>>();
        let mut tokens = Vec::new();
        let mut current = String::new();
        for (index, ch) in chars.iter().copied().enumerate() {
            if !ch.is_ascii_alphanumeric() {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                continue;
            }
            let prev = index
                .checked_sub(1)
                .and_then(|prev| chars.get(prev))
                .copied();
            let next = chars.get(index + 1).copied();
            let camel_boundary = ch.is_ascii_uppercase()
                && !current.is_empty()
                && prev.is_some_and(|prev| prev.is_ascii_alphanumeric())
                && (prev.is_some_and(|prev| prev.is_ascii_lowercase() || prev.is_ascii_digit())
                    || next.is_some_and(|next| next.is_ascii_lowercase()));
            if camel_boundary && !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            current.push(ch.to_ascii_lowercase());
        }
        if !current.is_empty() {
            tokens.push(current);
        }
        tokens
    }

    fn prompt_context_key_id(key: &str) -> String {
        let tokens = Self::prompt_visible_context_key_tokens(key);
        if tokens.is_empty() {
            key.to_ascii_lowercase()
        } else {
            tokens.join("_")
        }
    }

    fn prompt_visible_context_key(path: &[String], key: &str) -> Option<String> {
        let normalized = Self::prompt_context_key_id(key);
        let allowed = if path.is_empty() {
            matches!(
                normalized.as_str(),
                "mode"
                    | "raw_advice"
                    | "model_name"
                    | "author"
                    | "authority"
                    | "resources"
                    | "source_agent_id"
                    | "source_agent_workspace_id"
                    | "source_version"
                    | "advice_user_id"
                    | "current_agent"
                    | "authoring_context"
            )
        } else {
            match path.last().map(String::as_str) {
                Some("resources") => matches!(
                    normalized.as_str(),
                    "models" | "tools" | "skills" | "knowledge_bases"
                ),
                Some("models") => matches!(normalized.as_str(), "name" | "model_name"),
                Some("tools" | "skills" | "knowledge_bases") => normalized == "name",
                Some("current_agent") => matches!(
                    normalized.as_str(),
                    "agent_id"
                        | "name"
                        | "description"
                        | "model_name"
                        | "model_config_ref"
                        | "tool_names"
                        | "skill_names"
                        | "knowledge_base_names"
                        | "agent_md"
                ),
                Some("open_candidate") => matches!(
                    normalized.as_str(),
                    "agent_id" | "candidate_version" | "config"
                ),
                Some("config") => matches!(
                    normalized.as_str(),
                    "agent_id"
                        | "name"
                        | "description"
                        | "model_name"
                        | "model_config_ref"
                        | "tool_names"
                        | "skill_names"
                        | "knowledge_base_names"
                        | "agent_md"
                ),
                Some("authoring_context") => {
                    matches!(
                        normalized.as_str(),
                        "schema_version" | "recent_chat_context" | "open_candidate"
                    )
                }
                Some("recent_chat_context") => matches!(
                    normalized.as_str(),
                    "limit_turns" | "max_characters" | "truncated" | "messages"
                ),
                Some("messages") => matches!(normalized.as_str(), "role" | "content" | "truncated"),
                _ => false,
            }
        };
        allowed.then_some(normalized)
    }

    fn prompt_visible_context_shape(path: &[String], value: &Value) -> bool {
        match path {
            [field]
                if matches!(
                    field.as_str(),
                    "mode"
                        | "raw_advice"
                        | "model_name"
                        | "author"
                        | "authority"
                        | "source_agent_id"
                        | "source_agent_workspace_id"
                        | "source_version"
                        | "advice_user_id"
                ) =>
            {
                matches!(
                    value,
                    Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
                )
            }
            [field] if field == "resources" => value.is_object(),
            [parent, field]
                if parent == "resources"
                    && matches!(
                        field.as_str(),
                        "models" | "tools" | "skills" | "knowledge_bases"
                    ) =>
            {
                value.is_array()
            }
            [root, collection, field]
                if root == "resources"
                    && matches!(
                        collection.as_str(),
                        "models" | "tools" | "skills" | "knowledge_bases"
                    )
                    && matches!(field.as_str(), "name" | "model_name") =>
            {
                matches!(
                    value,
                    Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
                )
            }
            [field] if matches!(field.as_str(), "current_agent" | "authoring_context") => {
                value.is_object()
            }
            [root, field] if root == "authoring_context" && field == "open_candidate" => {
                value.is_object()
            }
            [root, field]
                if root == "current_agent"
                    && matches!(
                        field.as_str(),
                        "agent_id"
                            | "name"
                            | "description"
                            | "model_name"
                            | "model_config_ref"
                            | "agent_md"
                    ) =>
            {
                matches!(
                    value,
                    Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
                )
            }
            [root, candidate, field]
                if root == "authoring_context"
                    && candidate == "open_candidate"
                    && matches!(field.as_str(), "agent_id" | "candidate_version") =>
            {
                matches!(
                    value,
                    Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
                )
            }
            [root, candidate, field]
                if root == "authoring_context"
                    && candidate == "open_candidate"
                    && field == "config" =>
            {
                value.is_object()
            }
            [root, candidate, config, field]
                if root == "authoring_context"
                    && candidate == "open_candidate"
                    && config == "config"
                    && matches!(
                        field.as_str(),
                        "agent_id"
                            | "name"
                            | "description"
                            | "model_name"
                            | "model_config_ref"
                            | "agent_md"
                    ) =>
            {
                matches!(
                    value,
                    Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
                )
            }
            [root, candidate, config, field]
                if root == "authoring_context"
                    && candidate == "open_candidate"
                    && config == "config"
                    && matches!(
                        field.as_str(),
                        "tool_names" | "skill_names" | "knowledge_base_names"
                    ) =>
            {
                value.is_array()
            }
            [root, field]
                if root == "current_agent"
                    && matches!(
                        field.as_str(),
                        "tool_names" | "skill_names" | "knowledge_base_names"
                    ) =>
            {
                value.is_array()
            }
            [root, field] if root == "authoring_context" && field == "schema_version" => {
                matches!(
                    value,
                    Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
                )
            }
            [root, field] if root == "authoring_context" && field == "recent_chat_context" => {
                value.is_object()
            }
            [root, context, field]
                if root == "authoring_context"
                    && context == "recent_chat_context"
                    && matches!(
                        field.as_str(),
                        "limit_turns" | "max_characters" | "truncated"
                    ) =>
            {
                matches!(
                    value,
                    Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
                )
            }
            [root, context, field]
                if root == "authoring_context"
                    && context == "recent_chat_context"
                    && field == "messages" =>
            {
                value.is_array()
            }
            [root, context, messages, field]
                if root == "authoring_context"
                    && context == "recent_chat_context"
                    && messages == "messages"
                    && matches!(field.as_str(), "role" | "content" | "truncated") =>
            {
                matches!(
                    value,
                    Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
                )
            }
            _ => false,
        }
    }

    fn prompt_visible_context_string_limit(path: &[String]) -> Option<usize> {
        match path {
            [field] if field == "raw_advice" => None,
            [root, _] if root == "current_agent" => None,
            [root, candidate, config, field]
                if root == "authoring_context"
                    && candidate == "open_candidate"
                    && config == "config"
                    && field == "agent_md" =>
            {
                None
            }
            [root, context, messages, field]
                if root == "authoring_context"
                    && context == "recent_chat_context"
                    && messages == "messages"
                    && field == "content" =>
            {
                None
            }
            _ => Some(2_000),
        }
    }

    fn prompt_visible_context_array_limit(path: &[String]) -> Option<usize> {
        match path {
            [root, field]
                if root == "current_agent"
                    && matches!(
                        field.as_str(),
                        "tool_names" | "skill_names" | "knowledge_base_names"
                    ) =>
            {
                None
            }
            [root, candidate, config, field]
                if root == "authoring_context"
                    && candidate == "open_candidate"
                    && config == "config"
                    && matches!(
                        field.as_str(),
                        "tool_names" | "skill_names" | "knowledge_base_names"
                    ) =>
            {
                None
            }
            [root, context, field]
                if root == "authoring_context"
                    && context == "recent_chat_context"
                    && field == "messages" =>
            {
                None
            }
            _ => Some(24),
        }
    }

    fn prompt_visible_context_value(
        value: &Value,
        depth: usize,
        path: &mut Vec<String>,
    ) -> Option<Value> {
        const MAX_DEPTH: usize = 5;
        const MAX_OBJECT_FIELDS: usize = 48;

        match value {
            Value::Null | Value::Bool(_) | Value::Number(_) => Some(value.clone()),
            Value::String(text) => match Self::prompt_visible_context_string_limit(path) {
                None if !text.is_empty() => Some(Value::String(text.clone())),
                Some(max_chars) if text.chars().count() > max_chars => {
                    let truncated = text.chars().take(max_chars).collect::<String>();
                    Some(Value::String(format!("{truncated}...[truncated]")))
                }
                _ => Some(Value::String(text.clone())),
            },
            Value::Array(items) => {
                if depth >= MAX_DEPTH {
                    return None;
                }
                let max_items =
                    Self::prompt_visible_context_array_limit(path).unwrap_or(usize::MAX);
                let values = items
                    .iter()
                    .take(max_items)
                    .filter_map(|item| Self::prompt_visible_context_value(item, depth + 1, path))
                    .collect::<Vec<_>>();
                Some(Value::Array(values))
            }
            Value::Object(object) => {
                if depth >= MAX_DEPTH {
                    return None;
                }
                let mut visible = Map::new();
                for (key, value) in object {
                    if visible.len() >= MAX_OBJECT_FIELDS {
                        break;
                    }
                    let Some(normalized_key) = Self::prompt_visible_context_key(path, key) else {
                        continue;
                    };
                    path.push(normalized_key);
                    if Self::prompt_visible_context_shape(path, value)
                        && let Some(value) =
                            Self::prompt_visible_context_value(value, depth + 1, path)
                    {
                        visible.insert(key.clone(), value);
                    }
                    path.pop();
                }
                (!visible.is_empty()).then_some(Value::Object(visible))
            }
        }
    }

    fn agent_binding_turn_context_section(context: &Map<String, Value>) -> Option<String> {
        if context.is_empty() {
            return None;
        }
        let mut path = Vec::new();
        let payload_value =
            Self::prompt_visible_context_value(&Value::Object(context.clone()), 0, &mut path)?;
        let payload = serde_json::to_string(&payload_value)
            .expect("serde_json::Value serialization should not fail");
        Some(format!(
            "## Runtime Turn Context\nThe following JSON is provided by the runtime for this turn. Treat it as authoritative MOI context.\n```json\n{payload}\n```"
        ))
    }

    fn append_runtime_prompt_text(edge_profile: &mut Map<String, Value>, lane: &str, text: String) {
        if text.trim().is_empty() {
            return;
        }
        let entry = edge_profile
            .entry(lane.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        match entry {
            Value::Array(items) => items.push(Value::String(text)),
            Value::String(existing) if !existing.is_empty() => {
                let previous = std::mem::take(existing);
                *entry = Value::Array(vec![Value::String(previous), Value::String(text)]);
            }
            _ => {
                *entry = Value::Array(vec![Value::String(text)]);
            }
        }
    }

    fn append_runtime_required_prompt_text(edge_profile: &mut Map<String, Value>, text: String) {
        Self::append_runtime_prompt_text(
            edge_profile,
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_REQUIRED_TEXTS,
            text,
        );
    }

    fn append_runtime_volatile_prompt_text(edge_profile: &mut Map<String, Value>, text: String) {
        Self::append_runtime_prompt_text(
            edge_profile,
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_VOLATILE_TEXTS,
            text,
        );
    }

    fn apply_agent_binding_prompt_context(
        edge_profile: &mut Map<String, Value>,
        agent_binding_context: Option<&PreparedAgentBindingLoopContext>,
        runtime_system_prompt: Option<&str>,
        request_context: Option<&Map<String, Value>>,
    ) {
        let existing = edge_profile
            .get("system_prompt_override")
            .and_then(Value::as_str)
            .filter(|existing| !existing.is_empty())
            .map(str::to_string);
        let binding_section = agent_binding_context
            .map(|context| Self::agent_binding_prompt_section(&context.binding));
        let sections = [existing, binding_section]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        if !sections.is_empty() {
            let merged = sections.join("\n\n");
            edge_profile.insert("system_prompt_override".to_string(), Value::String(merged));
        }
        // Provider-owned runtime policy is not part of the editable agent
        // prompt. It is required control context: strict-history/cache paths
        // may reposition it, but must never drop or persist it as user history.
        if let Some(runtime_system_prompt) = runtime_system_prompt {
            Self::append_runtime_required_prompt_text(
                edge_profile,
                runtime_system_prompt.to_string(),
            );
        }
        if let Some(turn_context_section) = agent_binding_context
            .and(request_context)
            .and_then(Self::agent_binding_turn_context_section)
        {
            Self::append_runtime_volatile_prompt_text(edge_profile, turn_context_section);
        }
    }

    /// Build a [`ServerAgenticLoopHost`] for a single run.
    fn build_host(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        request: &ChatRequestData,
        edge_tools: Vec<Value>,
        edge_profile: Map<String, Value>,
        server_service_tool_catalog_enabled: bool,
        static_tool_catalog_admissible: bool,
        execution_bindings: Option<&ExecutionBindingSnapshot>,
        plan_resume_hint: Option<String>,
        plan_authoring_active: bool,
        task_board_resume_hint: Option<String>,
    ) -> server_loop_host::ServerAgenticLoopHost {
        let mut builder = ServerAgenticLoopHostBuilder::new(
            self.matrixone.clone(),
            self.encryptor.clone(),
            user_id.to_string(),
            session_id.to_string(),
        )
        .with_model(request.model.clone())
        .with_admitted_model_execution(request.admitted_model_execution.clone())
        .with_full_llm_capture(request.full_llm_capture)
        .with_edge_tools(edge_tools)
        .with_server_service_tool_catalog_enabled(server_service_tool_catalog_enabled)
        .with_static_tool_catalog_admissible(static_tool_catalog_admissible)
        .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
            self.shared_pool.is_some(),
            self.reflect_service.is_configured(),
        ))
        .with_edge_profile(edge_profile)
        .with_edge_callback_ledger(self.edge_callback_ledger.clone())
        .with_interaction_mode(request.interaction_mode)
        .with_turn_intent_policy(request.execution_policy.turn_intent)
        .with_interactive_client(request.interactive_client)
        .with_plan_resume_hint(plan_resume_hint)
        .with_plan_authoring_active(plan_authoring_active)
        .with_task_board_resume_hint(task_board_resume_hint);

        if let Some(pool) = &self.shared_pool {
            builder = builder.with_pool(pool.clone());
        }
        if let Some(svc) = &self.edge_dispatch_service {
            builder = builder.with_edge_dispatch_service(Arc::clone(svc));
        }
        if let Some(snapshot) = execution_bindings {
            builder = builder.with_execution_binding_snapshot(snapshot.clone());
        }
        // Wire one shared agent-progress broadcaster for delegation and
        // dynamic `agent(action='spawn')` trees so Web SSE observes a single lineage.
        builder = builder
            .with_progress_broadcaster(self.dynamic_agent_progress_broadcaster())
            .with_progress_root_run_id(run_id.to_string());
        if let Some(ref de) = self.delegation_engine {
            if let Some(store) = de.prefix_store() {
                builder = builder.with_prefix_store(Some(Arc::clone(store)));
            }
        }
        // Share the tool execution service's disabled tool-offer set so the LLM
        // surface excludes admin-disabled tool offers (not just dispatch-rejected).
        if let Some(ref shared_tes) = self.tool_execution_service {
            builder = builder
                .with_disabled_tool_offers(shared_tes.disabled_tool_offers_handle())
                .with_provider_capabilities(shared_tes.provider_capabilities_handle())
                .with_provider_allowed_tools(shared_tes.provider_allowed_tools_handle());
        }
        // Wire test LLM rounds from request context (E2E test hook).
        #[cfg(feature = "bridge-e2e-hooks")]
        if let Some(rounds) = request
            .context
            .as_ref()
            .and_then(|c| c.get("test_llm_rounds"))
            .and_then(Value::as_array)
            .cloned()
        {
            builder = builder.with_test_llm_rounds(rounds);
        }
        builder.build()
    }

    fn configure_host_approval_audit_context(
        &self,
        host: &mut server_loop_host::ServerAgenticLoopHost,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        turn: u32,
        agent_id: Option<String>,
    ) {
        let Some(writer) = self.auxiliary_event_writer.clone() else {
            return;
        };
        host.set_approval_audit_context(
            astra_turn_core::cloud_tool_delivery::ApprovalAuditContext {
                user_id: user_id.to_string(),
                session_id: session_id.to_string(),
                run_id: run_id.to_string(),
                turn,
                agent_id,
                parent_event_id: None,
                parent_event_ids: Vec::new(),
                causal_chain_id: server_loop_causal_chain_id("server-loop-tools"),
                auxiliary_event_writer: writer,
            },
        );
    }

    async fn session_resume_hydration_hint_for_session(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        resume_requested: bool,
    ) -> Option<String> {
        if !resume_requested {
            return None;
        }

        let Some(shared) = &self.shared_pool else {
            tracing::warn!(
                target: "astra_runtime::resume_hydration",
                user_id,
                session_id,
                "resume hydration skipped: shared MatrixOne pool is not configured"
            );
            return None;
        };

        let restore =
            astra_services::session_restore::HybridRestoreService::new(shared.get().clone());
        match restore.restore_session(user_id, session_id).await {
            Ok(Some(restored)) => {
                if let Some(hint) = Self::session_resume_hydration_hint_or_invalid_failure(
                    user_id,
                    session_id,
                    &restored.conversation_messages,
                    &[],
                ) {
                    return Some(hint);
                }
                let transcript_messages = self
                    .restore_transcript_prompt_messages(
                        user_id,
                        session_id,
                        run_id,
                        "hybrid_restore_unusable",
                    )
                    .await;
                if let Some(hint) = Self::session_resume_hydration_hint_or_invalid_failure(
                    user_id,
                    session_id,
                    &restored.conversation_messages,
                    &transcript_messages,
                ) {
                    return Some(hint);
                }
                tracing::warn!(
                    target: "astra_runtime::resume_hydration",
                    user_id,
                    session_id,
                    "resume hydration degraded: restored session has no prompt-facing transcript"
                );
                Some(
                    astra_turn_core::resume_hydration::build_resume_hydration_failure_hint(
                        "resume restored session metadata but no prompt-facing transcript/history",
                    ),
                )
            }
            Ok(None) => {
                let transcript_messages = self
                    .restore_transcript_prompt_messages(
                        user_id,
                        session_id,
                        run_id,
                        "hybrid_restore_empty",
                    )
                    .await;
                if let Some(hint) = Self::session_resume_hydration_hint_or_invalid_failure(
                    user_id,
                    session_id,
                    &[],
                    &transcript_messages,
                ) {
                    return Some(hint);
                }
                tracing::warn!(
                    target: "astra_runtime::resume_hydration",
                    user_id,
                    session_id,
                    "resume hydration skipped: session restore returned no resumable state"
                );
                Some(
                    astra_turn_core::resume_hydration::build_resume_hydration_failure_hint(
                        "session restore returned no resumable state",
                    ),
                )
            }
            Err(error) => {
                let transcript_messages = self
                    .restore_transcript_prompt_messages(
                        user_id,
                        session_id,
                        run_id,
                        "hybrid_restore_failed",
                    )
                    .await;
                if let Some(hint) = Self::session_resume_hydration_hint_or_invalid_failure(
                    user_id,
                    session_id,
                    &[],
                    &transcript_messages,
                ) {
                    return Some(hint);
                }
                tracing::warn!(
                    target: "astra_runtime::resume_hydration",
                    user_id,
                    session_id,
                    error = %error,
                    "resume hydration skipped: session restore failed"
                );
                Some(
                    astra_turn_core::resume_hydration::build_resume_hydration_failure_hint(
                        "session restore failed and transcript fallback was unavailable",
                    ),
                )
            }
        }
    }

    async fn session_has_prior_prompt_history(&self, user_id: &str, session_id: &str) -> bool {
        let Some(shared) = &self.shared_pool else {
            return false;
        };
        let pool = shared.get();

        match sqlx::query(
            "SELECT 1 AS present
             FROM conversation_log
             WHERE user_id = ? AND session_id = ?
             LIMIT 1",
        )
        .bind(user_id)
        .bind(session_id)
        .fetch_optional(pool)
        .await
        {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    target: "astra_runtime::resume_hydration",
                    user_id,
                    session_id,
                    error = %error,
                    "failed to check canonical conversation history before resume hydration"
                );
            }
        }

        match sqlx::query(PROMPT_HISTORY_TRANSCRIPT_EXISTS_SQL)
            .bind(session_id)
            .bind(user_id)
            .fetch_optional(pool)
            .await
        {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(
                    target: "astra_runtime::resume_hydration",
                    user_id,
                    session_id,
                    error = %error,
                    "failed to check transcript history before resume hydration"
                );
                false
            }
        }
    }

    fn session_resume_hydration_hint_from_sources(
        primary_messages: &[Value],
        transcript_messages: &[Value],
    ) -> Result<Option<String>, astra_turn_types::UserTurnSemanticsError> {
        if let Some(hint) =
            astra_turn_core::resume_hydration::build_resume_hydration_hint_from_messages(
                primary_messages,
            )?
        {
            return Ok(Some(hint));
        }
        astra_turn_core::resume_hydration::build_resume_hydration_hint_from_messages(
            transcript_messages,
        )
    }

    fn session_resume_hydration_hint_or_invalid_failure(
        user_id: &str,
        session_id: &str,
        primary_messages: &[Value],
        transcript_messages: &[Value],
    ) -> Option<String> {
        match Self::session_resume_hydration_hint_from_sources(
            primary_messages,
            transcript_messages,
        ) {
            Ok(hint) => hint,
            Err(error) => {
                tracing::warn!(
                    target: "astra_runtime::resume_hydration",
                    user_id,
                    session_id,
                    error = %error,
                    "resume hydration degraded: restored typed turn metadata is invalid"
                );
                Some(
                    astra_turn_core::resume_hydration::build_resume_hydration_failure_hint(
                        "restored resume source contains invalid typed turn metadata",
                    ),
                )
            }
        }
    }

    async fn task_board_resume_hint_for_session(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Option<String> {
        let Some(shared) = &self.shared_pool else {
            return None;
        };
        let store: Arc<dyn TaskStore> =
            match MatrixOneTaskStore::from_shared_for_user(shared, user_id) {
                Ok(store) => Arc::new(store),
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        user_id = %user_id,
                        error = %error,
                        "failed to construct user-scoped task store for resume hint"
                    );
                    return None;
                }
            };
        let manager = TaskManager::new(session_id.to_string(), store);
        match manager.load_tasks().await {
            Ok(tasks) => format_task_board_resume_hint(&tasks),
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    user_id = %user_id,
                    error = %error,
                    "failed to load task board resume hint for Cloud turn"
                );
                Some(format!(
                    "Task board state could not be loaded for this turn: {error}. \
                     Do not assume the task board is empty; avoid creating duplicate tasks and surface the load failure to the user."
                ))
            }
        }
    }

    /// Build the initial [`AgenticLoopState`] from a chat request.
    ///
    /// `workspace_override` — when the server provisions a workspace (web-agent
    /// mode, no CLI edge), pass it here so stop hooks and skill hooks are loaded
    /// from the provisioned directory instead of requiring `edge_profile.cwd`.
    #[cfg(test)]
    fn build_initial_state(
        &self,
        user_id: &str,
        request: &ChatRequestData,
        session_id: &str,
        run_id: &str,
        workspace_override: Option<&std::path::Path>,
        execution_bindings: Option<&ExecutionBindingSnapshot>,
        cancel_token: Option<Arc<CancellationToken>>,
    ) -> AgenticLoopState {
        let request_constraints = match Self::try_request_constraints(request) {
            Ok(c) => c,
            Err(err) => {
                tracing::error!(
                    error = %err,
                    "request constraints failed validation in build_initial_state; upstream caller should validate first",
                );
                RequestConstraints::default()
            }
        };
        let edge_context = match Self::extract_edge_context(request) {
            Ok(context) => context,
            Err(err) => {
                tracing::error!(
                    error = %err.1.0.detail,
                    "edge context failed validation in test-only build_initial_state; production callers reject this before agent start",
                );
                EdgeContext::default()
            }
        };
        self.build_initial_state_inner(
            user_id,
            request,
            session_id,
            run_id,
            workspace_override,
            execution_bindings,
            cancel_token,
            None,
            request_constraints,
            &edge_context,
            None,
            None,
            None,
        )
    }

    fn build_initial_state_inner(
        &self,
        user_id: &str,
        request: &ChatRequestData,
        session_id: &str,
        run_id: &str,
        workspace_override: Option<&std::path::Path>,
        execution_bindings: Option<&ExecutionBindingSnapshot>,
        cancel_token: Option<Arc<CancellationToken>>,
        interaction_sink: Option<Arc<dyn server_loop_host::HostInteractionSink>>,
        request_constraints: RequestConstraints,
        edge_context: &EdgeContext,
        edge_profile_override: Option<&Map<String, Value>>,
        request_scoped_skill_resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
        agent_binding_context: Option<&PreparedAgentBindingLoopContext>,
    ) -> AgenticLoopState {
        use astra_pipeline::step_protocol::InMemoryIdempotencyCache;
        use astra_text_utils::semantic_dedup::SemanticDedup;
        use astra_turn_core::chat_turn_heuristics::infer_task_execution_profile;
        use astra_turn_core::stop_hooks_yaml::{
            detect_turn_hook_sets, is_plan_subtask_from_chat_context, project_root_for_stop_hooks,
        };

        let (skill_registry, skill_resolver) = if let Some(binding_context) = agent_binding_context
        {
            (None, binding_context.skill_resolver.clone())
        } else if let Some(skill_resolver) = request_scoped_skill_resolver {
            (None, Some(skill_resolver))
        } else {
            let (skill_registry, raw_skill_resolver) =
                build_server_skill_resolver(self.skill_service.clone(), user_id);
            let skill_resolver =
                    apply_normalized_skill_allowlist(raw_skill_resolver, &request_constraints)
                        .unwrap_or_else(|err| {
                            tracing::error!(
                                error = %err,
                                "skill allowlist failed in build_initial_state; upstream caller should validate first",
                            );
                            None
                        });
            (skill_registry, skill_resolver)
        };
        use astra_turn_core::turn_guard::TurnGuard;

        let prompt_user_message = request.message.trim().to_string();
        let prompt_user_intent = request
            .user_intent
            .as_deref()
            .filter(|intent| !intent.trim().is_empty())
            .unwrap_or(request.message.as_str())
            .trim()
            .to_string();
        let user_message = json!({
            "role": "user",
            "content": prompt_user_message,
        });

        let task_profile = infer_task_execution_profile(&prompt_user_message);
        let runtime_turn_ceiling = astra_config::runtime_config::RuntimeConfig::cached()
            .runtime_limits
            .resolve_turn_ceiling(is_plan_subtask_from_chat_context(&request.context));
        let mut requested_budget = request.execution_budget.as_ref().map(|budget| {
            astra_turn_core::chat_turn_heuristics::AgenticTurnBudgetOverride {
                initial_turns: budget.initial_turns.map(|value| value as usize),
                hard_turn_limit: budget.hard_turn_limit.map(|value| value as usize),
            }
        });
        if let Some(max_steps) =
            agent_binding_context.and_then(|context| context.binding.runtime_policy.max_steps)
        {
            let max_steps = max_steps as usize;
            let initial_turns = requested_budget
                .as_ref()
                .and_then(|budget| budget.initial_turns)
                .map(|initial| initial.min(max_steps));
            requested_budget = Some(
                astra_turn_core::chat_turn_heuristics::AgenticTurnBudgetOverride {
                    initial_turns,
                    hard_turn_limit: Some(max_steps),
                },
            );
        }
        let agentic_turn_budget =
            astra_turn_core::chat_turn_heuristics::resolve_agentic_turn_budget(
                task_profile,
                runtime_turn_ceiling,
                requested_budget,
            );
        let max_turns = agentic_turn_budget.initial_turns;
        // Use edge profile's git_root/cwd if available; fall back to provisioned
        // server workspace so web-agent sessions still load stop-hooks.yaml.
        let project_root_buf = project_root_for_stop_hooks(edge_context)
            .or_else(|| workspace_override.map(|p| p.to_path_buf()));
        let hook_sets = project_root_buf
            .as_ref()
            .map(|root| {
                detect_turn_hook_sets(
                    root.as_path(),
                    task_profile,
                    is_plan_subtask_from_chat_context(&request.context),
                )
            })
            .unwrap_or_default();
        let workspace_root_hint = project_root_buf.map(|p| p.to_string_lossy().into_owned());
        let (tool_event_hooks, session_event_hooks) = workspace_root_hint
            .as_ref()
            .map(|root| crate::skills::hooks::load_all_hooks(std::path::Path::new(root)))
            .unwrap_or_default();
        let thinking_config =
            Self::thinking_from_chat_context(&request.context, request.model.as_deref());
        let root_permissions =
            Self::inherited_permissions_from_request(request, &request_constraints);
        let root_permission_context = Some(PermissionSyncContext::shared(root_permissions.clone()));

        // Create harness sink early so sub-run executors can share it.
        #[cfg(feature = "harness")]
        let (harness_server_sink, harness_sink_arc): (
            Option<std::sync::Arc<crate::server::harness::server_sink::ServerSnapshotSink>>,
            Option<std::sync::Arc<dyn astra_harness::SnapshotSink>>,
        ) = if self.harness_registry.is_some() {
            let mut raw_sink = crate::server::harness::server_sink::ServerSnapshotSink::new(
                session_id.to_string(),
                String::new(),
            );
            if let Some(ref pool) = self.shared_pool {
                raw_sink = raw_sink.with_pool(pool.get().clone());
            }
            let concrete = std::sync::Arc::new(raw_sink);
            let dyn_sink = concrete.clone() as std::sync::Arc<dyn astra_harness::SnapshotSink>;
            (Some(concrete), Some(dyn_sink))
        } else {
            (None, None)
        };

        // Build the server-side skill fork executor so skills with
        // execution_context: Fork can run in isolated sub-agent loops.
        let edge_tools = edge_context.edge_tools.clone();
        let edge_profile = edge_profile_override
            .cloned()
            .unwrap_or_else(|| edge_context.edge_profile.to_map());
        let memory_extraction_service = self.memory_extraction_service.as_ref().and_then(|svc| {
            match svc.scoped_to_owner(user_id) {
                Ok(scoped) => Some(scoped),
                Err(error) => {
                    tracing::error!(
                        user_id,
                        session_id,
                        error = %error,
                        "session-memory extraction disabled because the transport could not bind the authenticated owner"
                    );
                    None
                }
            }
        });
        let skill_executor = build_server_skill_executor(
            &self.matrixone,
            &self.encryptor,
            self.shared_pool.as_ref(),
            request.model.as_deref(),
            request.admitted_model_execution.as_ref(),
            &edge_tools,
            &edge_profile,
            execution_bindings,
            &request.forward_headers,
            request_constraints.clone(),
            root_permissions.clone(),
            skill_resolver.clone(),
            Arc::clone(&self.reflect_service),
            session_id,
            self.edge_connection_pool.as_ref(),
            cancel_token,
            memory_extraction_service.as_ref(),
            interaction_sink,
            #[cfg(feature = "harness")]
            harness_sink_arc.as_ref(),
        );
        let resolved_tool_policy = astra_config::runtime_config::RuntimeConfig::load()
            .tool_selection
            .resolve_for_model(request.model.as_deref());
        AgenticLoopState {
            messages: vec![user_message],
            run_transcript_capture: None,
            volatile_pending: Vec::new(),
            recent_rounds: Vec::new(),
            tool_results: Vec::new(),
            current_session_id: Some(session_id.to_string()),
            current_run_id: Some(run_id.to_string()),
            inference_purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
            context_manifest_pool: self.shared_pool.clone(),
            context_manifest_user_id: None,
            context_manifest_model_name: request.model.clone(),
            runtime_manifest: None,
            recursion_depth: 0,
            final_text: String::new(),
            final_text_streamed: false,
            final_output_ready_notified: false,
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            total_observation_tool_calls: 0,
            has_any_usage: false,
            last_finish_reason: None,
            max_turns,
            remaining_turns: max_turns,
            turn_budget_hint_emitted_90: false,
            turn_budget_hint_emitted_50: false,
            turn_budget_hint_emitted_20: false,
            agentic_turn_budget,
            budget_policy: None,
            current_round_index: 0,
            llm_rounds_completed: 0,
            last_request_message_count: None,
            turn_guard: TurnGuard::with_profile(task_profile),
            restricted_tools: std::collections::HashSet::new(),
            boosted_tools: std::collections::HashSet::new(),
            widen_selection_pending: false,
            step_recorder: StepRecorder::new(user_id, session_id, run_id),
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(0.75),
            call_counts: HashMap::new(),
            // Per-model workflow-guard policy (see
            // `ToolSelectionConfig::resolve_for_model`). Built-in profiles
            // give stronger models (opus/sonnet-4) more rope than haiku.
            // Security guards (shell_obfuscation, destructive_sql) are
            // unaffected and stay uniform across models.
            max_identical_tool_calls: resolved_tool_policy.max_identical_tool_calls,
            max_tools_per_turn: resolved_tool_policy.max_tools_per_turn,
            repeated_cache_hit_suppression: resolved_tool_policy.repeated_cache_hit_suppression,
            max_consecutive_empty_name: resolved_tool_policy.max_consecutive_empty_name,
            stall: Default::default(),
            telemetry: Default::default(),
            skills: SkillState {
                registry_for_activation: if request_constraints.allowed_skills.is_some() {
                    None
                } else {
                    skill_registry
                },
                resolver: skill_resolver,
                executor: skill_executor,
                request_constraints,
                quality_tracker: crate::skills::quality::SkillQualityTracker::new(),
                improvement_tracker: astra_skills::improvement::ImprovementTracker::new(),
                tool_event_hooks,
                session_event_hooks,
                ..Default::default()
            },
            hooks: StopHookState {
                stop_hooks: hook_sets.stop_hooks,
                teammate_idle_hooks: hook_sets.teammate_idle_hooks,
                workspace_root_hint,
                forward_headers: request.forward_headers.clone(),
                admitted_model_execution: request.admitted_model_execution.clone(),
                ..Default::default()
            },
            cancellation: Default::default(),
            messaging: Default::default(),
            user_intents: Default::default(),
            error_recovery: Default::default(),
            run_control: None,
            pipeline_session: Some(
                astra_turn_core::pipeline_session::PipelineSession::new_with_current_date(
                    astra_turn_core::pipeline_config::PipelineConfig::default(),
                    crate::turn::session_current_date::resolve_session_current_date(session_id),
                ),
            ),
            message: prompt_user_message.clone(),
            user_intent: prompt_user_intent,
            recent_tools: Vec::new(),
            has_prior_assistant_turn: false,
            turn_intent: None,
            task_profile,
            last_turn_policy: crate::turn::agentic_loop::host::TurnInteractionPolicy::default(),
            api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None)
                .expect("valid dummy URL"),
            api_token: String::new(),
            delegation_engine: None,
            delegations_this_turn: 0,
            delegation_chain: Vec::new(),
            self_agent_id: "orchestrator".to_string(),
            project_context: None,
            checkpoint_gate: None,
            last_llm_context_manifest_trace: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            compaction_effectiveness: Default::default(),
            pinned_tool_schema_tokens: 0,
            sticky_tool_schemas: Vec::new(),
            max_turn_input_tokens: astra_core::RuntimeLimits::global().max_turn_input_tokens,
            budget_wrapup_injected: false,
            budget_wrapup_ignored_rounds: 0,
            compact_tier_applied: astra_turn_core::compaction_types::CompactionTier::Normal,
            skill_produced_output: false,
            max_cumulative_tokens: 0,
            thinking: thinking_config,
            recent_file_reads: Vec::new(),
            permission_context: root_permission_context,
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            tool_budget_override: None,
            recent_tactical_actions: Vec::new(),
            runtime_tool_executor: None,
            interruption: None,
            session_facts: Default::default(),
            memory_extraction_service,
            observation_journal: Default::default(),
            observation_store: None,
            session_memory_state: Default::default(),
            session_memory_llm_params: None,
            compact_strategy: astra_turn_core::microcompact::CompactStrategy::from_provider_hint(
                request.model.as_deref().unwrap_or(""),
            ),
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
            session_turn: 0,
            bridge_turn_chain_id: Some(format!("{}:harness", session_id)),
            bridge_user_query_event_id: None,
            turn_event_buffer: None,
            harness: {
                #[cfg(feature = "harness")]
                {
                    if let (Some(registry), Some(server_sink), Some(sink_arc)) = (
                        &self.harness_registry,
                        harness_server_sink,
                        harness_sink_arc,
                    ) {
                        let broadcaster_tx = server_sink.broadcaster_sender();
                        let limits = astra_harness::HarnessLimits {
                            max_turns: if max_turns > 0 {
                                Some(max_turns as u32)
                            } else {
                                None
                            },
                            ..Default::default()
                        };
                        let kernel = std::sync::Arc::new(
                            astra_harness::StandardKernel::configured(sink_arc.clone(), limits),
                        );
                        registry.register_with_broadcast(
                            session_id.to_string(),
                            sink_arc.clone(),
                            broadcaster_tx,
                        );
                        let mut slot = crate::turn::harness_adapter::HarnessSlot::new(
                            kernel as std::sync::Arc<dyn astra_harness::HarnessKernel>,
                            sink_arc,
                        );
                        slot.registry = Some(registry.clone());
                        slot.session_id_for_cleanup = Some(session_id.to_string());
                        slot.server_sink = Some(server_sink);
                        slot
                    } else {
                        crate::turn::harness_adapter::HarnessSlot::empty()
                    }
                }
                #[cfg(not(feature = "harness"))]
                {
                    crate::turn::harness_adapter::HarnessSlot::empty()
                }
            },
        }
    }

    fn thinking_from_chat_context(
        context: &Option<Map<String, Value>>,
        model: Option<&str>,
    ) -> astra_turn_core::thinking_config::ThinkingConfig {
        if let Some(value) = context.as_ref().and_then(|ctx| ctx.get("thinking")) {
            return astra_turn_core::thinking_config::ThinkingConfig::from_payload_value(value);
        }
        model
            .map(|name| astra_turn_core::thinking_config::resolve_model_thinking(name).1)
            .unwrap_or_default()
    }
    /// Extract edge tools from the request context, or provide empty defaults.
    /// Parse the request context into a typed [`EdgeContext`].
    fn extract_edge_context(
        request: &ChatRequestData,
    ) -> Result<EdgeContext, (StatusCode, Json<ErrorResponse>)> {
        request.context.as_ref().map_or_else(
            || Ok(EdgeContext::default()),
            |context| {
                EdgeContext::from_context_map(context).map_err(|error| {
                    error_response(
                        StatusCode::BAD_REQUEST,
                        format!("invalid edge context: {error}"),
                    )
                })
            },
        )
    }

    fn server_service_tool_catalog_enabled_for_request(
        agent_binding_mode: bool,
        _has_runtime_executor_tools: bool,
    ) -> bool {
        // Edge, sandbox, and managed-runtime providers add workspace/process
        // capacity. They must not replace the server-owned backbone:
        // task/session lifecycle, introspect/reflect, planning, memory, web/API
        // services, and policy/audit control-plane tools.
        !agent_binding_mode
    }

    /// Extract edge tools from the request context, or provide empty defaults.
    #[cfg(test)]
    fn extract_edge_tools(
        request: &ChatRequestData,
    ) -> Result<Vec<Value>, (StatusCode, Json<ErrorResponse>)> {
        Ok(Self::extract_edge_context(request)?.edge_tools)
    }

    /// Extract edge profile from the request context, or provide empty defaults.
    #[cfg(test)]
    fn extract_edge_profile(
        request: &ChatRequestData,
    ) -> Result<Map<String, Value>, (StatusCode, Json<ErrorResponse>)> {
        Ok(Self::extract_edge_context(request)?.edge_profile.to_map())
    }

    /// Provision a cloud workspace record for orchestrator-managed workspaces.
    async fn provision_cloud_workspace_record(
        &self,
        user_id: &str,
        session_id: &str,
        request: &ChatRequestData,
        run_id: &str,
    ) -> Result<Option<RuntimeWorkspaceRecord>, (StatusCode, Json<ErrorResponse>)> {
        let Some(provision_request) =
            cloud_workspace_provision_request_from_request(request, run_id)?
        else {
            return Ok(None);
        };
        let record = CloudWorkspaceProvisioner::from_env()
            .provision(provision_request)
            .await
            .map_err(cloud_workspace_provision_error)?;
        if let Err(error) = self
            .persist_workspace_record(user_id, session_id, run_id, &record)
            .await
        {
            self.cleanup_cloud_workspace_after_failed_start(
                user_id,
                session_id,
                run_id,
                &record,
                format!(
                    "workspace record persistence failed before orchestrator binding: {}",
                    error.1.0.detail
                ),
            )
            .await;
            return Err(error);
        }
        Ok(Some(record))
    }

    async fn persist_workspace_record(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        record: &RuntimeWorkspaceRecord,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let Some(store) = self.workspace_record_store.as_ref() else {
            return Ok(());
        };
        store
            .upsert_workspace_record(StoredWorkspaceRecordEntry::new(
                user_id.to_string(),
                Some(session_id.to_string()),
                Some(run_id.to_string()),
                record.clone(),
            ))
            .await
            .map_err(workspace_record_store_error)
    }

    async fn cleanup_cloud_workspace_after_failed_start(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        record: &RuntimeWorkspaceRecord,
        failure_message: String,
    ) {
        Self::cleanup_cloud_workspace_with_debt(
            self.workspace_record_store.clone(),
            user_id,
            session_id,
            run_id,
            record,
            RuntimeCleanupReason::Failed,
            failure_message,
        )
        .await;
    }

    async fn cleanup_cloud_workspace_after_terminal_run(
        workspace_record_store: Option<Arc<dyn WorkspaceStateStore>>,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        record: &RuntimeWorkspaceRecord,
        status: &RunStatus,
    ) {
        let Some(reason) = cleanup_reason_for_terminal_run_status(status) else {
            return;
        };
        Self::cleanup_cloud_workspace_with_debt(
            workspace_record_store,
            user_id,
            session_id,
            run_id,
            record,
            reason,
            format!("run ended with status {}", status.as_str()),
        )
        .await;
    }

    async fn cleanup_cloud_workspace_with_debt(
        workspace_record_store: Option<Arc<dyn WorkspaceStateStore>>,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        record: &RuntimeWorkspaceRecord,
        reason: RuntimeCleanupReason,
        failure_message: String,
    ) {
        match CloudWorkspaceProvisioner::from_env()
            .cleanup(record, reason)
            .await
        {
            Ok(()) => {
                if let Some(store) = workspace_record_store.as_ref()
                    && let Err(error) = store
                        .delete_workspace_record(user_id, &record.workspace_id)
                        .await
                {
                    tracing::warn!(
                        target: "astra_runtime::run_lifecycle",
                        workspace_id = %record.workspace_id,
                        run_id = %run_id,
                        error = %error,
                        "cleaned provisioned cloud workspace but failed to remove workspace record"
                    );
                }
                tracing::info!(
                    target: "astra_runtime::run_lifecycle",
                    workspace_id = %record.workspace_id,
                    run_id = %run_id,
                    "cleaned provisioned cloud workspace"
                );
            }
            Err(cleanup_error) => {
                tracing::warn!(
                    target: "astra_runtime::run_lifecycle",
                    workspace_id = %record.workspace_id,
                    run_id = %run_id,
                    error = %cleanup_error,
                    failure = %failure_message,
                    "failed to clean provisioned cloud workspace"
                );
                Self::record_workspace_cleanup_debt_in_store(
                    workspace_record_store,
                    user_id,
                    session_id,
                    run_id,
                    record,
                    cleanup_error.reason,
                    format!(
                        "{failure_message}; cleanup failed: {}",
                        cleanup_error.message
                    ),
                )
                .await;
            }
        }
    }

    async fn record_workspace_cleanup_debt_in_store(
        workspace_record_store: Option<Arc<dyn WorkspaceStateStore>>,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        record: &RuntimeWorkspaceRecord,
        reason: RuntimeCleanupReason,
        message: String,
    ) {
        let Some(store) = workspace_record_store.as_ref() else {
            tracing::warn!(
                target: "astra_runtime::run_lifecycle",
                workspace_id = %record.workspace_id,
                run_id = %run_id,
                "workspace cleanup debt store is unavailable"
            );
            return;
        };
        if let Err(error) = store
            .record_cleanup_debt(WorkspaceCleanupDebtEntry::new(
                user_id.to_string(),
                Some(session_id.to_string()),
                Some(run_id.to_string()),
                record.clone(),
                reason,
                message,
            ))
            .await
        {
            tracing::warn!(
                target: "astra_runtime::run_lifecycle",
                workspace_id = %record.workspace_id,
                run_id = %run_id,
                error = %error,
                "failed to persist workspace cleanup debt"
            );
        }
    }

    /// Provision a sandboxed workspace directory for server-side tool execution.
    fn provision_server_workspace(
        &self,
        session_id: &str,
    ) -> Result<std::path::PathBuf, (StatusCode, Json<ErrorResponse>)> {
        let record = ServerWorkspaceProvisioner::from_env()
            .provision(session_id)
            .map_err(server_workspace_provision_error)?;
        Ok(record.root)
    }

    /// Collect run events into SSE-compatible format.
    fn format_run_events(events: &[Value], start_index: usize) -> Vec<Value> {
        events
            .iter()
            .enumerate()
            .map(|(i, ev)| {
                let mut out = ev.clone();
                if let Some(obj) = out.as_object_mut() {
                    obj.insert("index".to_string(), json!(start_index + i));
                }
                out
            })
            .collect()
    }

    fn durable_event_payload(event: &Value) -> Option<&Map<String, Value>> {
        if event.get("event_type").is_some() {
            event.get("data").and_then(Value::as_object)
        } else {
            event.as_object()
        }
    }

    fn durable_run_execution_binding_snapshot(
        run: &DurableRunRecord,
    ) -> RunExecutionBindingSnapshot {
        let mut snapshot = RunExecutionBindingSnapshot::default();
        for event in &run.events {
            let Some(payload) = Self::durable_event_payload(event) else {
                continue;
            };
            if let Some(workspace) = payload
                .get("workspace")
                .filter(|value| value.is_object())
                .cloned()
            {
                snapshot.workspace = Some(workspace);
            }
            if let Some(executor) = payload
                .get("executor")
                .filter(|value| value.is_object())
                .cloned()
            {
                snapshot.executor = Some(executor);
            }
            if let Some(transport) = payload.get("transport").and_then(Value::as_str) {
                snapshot.transport = Some(transport.to_string());
            }
        }
        snapshot
    }

    fn durable_status_record(run: &DurableRunRecord) -> RunStatusRecord {
        let binding = Self::durable_run_execution_binding_snapshot(run);
        RunStatusRecord {
            run_id: run.run_id.clone(),
            session_id: run.session_id.clone(),
            parent_run_id: run.parent_run_id.clone(),
            root_run_id: run.root_run_id.clone(),
            depth: run.depth,
            status: run.status.clone(),
            waiting_for: run.waiting_for.clone(),
            events_count: run.events.len() as i64,
            workspace: binding.workspace,
            executor: binding.executor,
            transport: binding.transport,
        }
    }

    fn durable_stream_events(run: &DurableRunRecord, last_index: u32) -> Vec<Value> {
        let offset = last_index as usize;
        if offset < run.events.len() {
            Self::format_run_events(&run.events[offset..], offset)
        } else {
            Vec::new()
        }
    }

    async fn persist_started_run_quota_rejection(
        run_engine: &RunEngine,
        runs: &Arc<RwLock<HashMap<String, RunState>>>,
        user_id: &str,
        run_id: &str,
        limit: astra_services::resource_governor::ResourceLimitKind,
        reason: &str,
    ) -> Option<Vec<Value>> {
        let terminal_events = per_user_run_quota_terminal_events(limit, reason);
        let committed = match run_engine
            .transition_status_with_events_if_current(
                user_id,
                run_id,
                &[STATUS_RUNNING, STATUS_PAUSED, STATUS_WAITING],
                STATUS_FAILED,
                None,
                Some(reason),
                &terminal_events,
            )
            .await
        {
            Ok(true) => true,
            Ok(false) => {
                tracing::warn!(
                    target: "astra_runtime::run_lifecycle",
                    run_id,
                    limit = %limit.as_str(),
                    "skipping quota rejection terminal events after stale status CAS"
                );
                false
            }
            Err(error) => {
                tracing::warn!(
                    target: "astra_runtime::run_lifecycle",
                    run_id,
                    limit = %limit.as_str(),
                    error = %error,
                    "failed to persist quota rejection status/events transition"
                );
                false
            }
        };
        if !committed {
            return None;
        }

        let terminal_events = terminal_events.to_vec();
        if let Some(run) = runs.write().await.get_mut(run_id) {
            if run.status.try_transition(&RunStatus::Failed).is_ok() {
                run.status = RunStatus::Failed;
            }
            run.events.extend(terminal_events.iter().cloned());
            run.live_tx = None;
        }
        Some(terminal_events)
    }

    fn durable_recent_events(run: &DurableRunRecord, limit: u32) -> Vec<Value> {
        let capped = limit.clamp(1, MAX_DURABLE_RUN_PROJECTION_RECENT_EVENTS) as usize;
        let offset = run.events.len().saturating_sub(capped);
        Self::format_run_events(&run.events[offset..], offset)
    }

    async fn load_durable_run_for_user(
        &self,
        run_id: &str,
        user_id: &str,
    ) -> Result<Option<DurableRunRecord>, (StatusCode, Json<ErrorResponse>)> {
        let run = self
            .run_engine
            .load_run(user_id, run_id)
            .await
            .map_err(|error| {
                error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("Failed to load durable run state: {error}"),
                )
            })?;
        Ok(run)
    }

    async fn require_durable_run_for_user(
        &self,
        run_id: &str,
        user_id: &str,
    ) -> Result<DurableRunRecord, (StatusCode, Json<ErrorResponse>)> {
        self.load_durable_run_for_user(run_id, user_id)
            .await?
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Run not found"))
    }

    fn run_status_from_durable(
        status: &str,
    ) -> Result<RunStatus, (StatusCode, Json<ErrorResponse>)> {
        RunStatus::from_durable_status(status).ok_or_else(|| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Invalid durable run status '{status}'"),
            )
        })
    }

    fn run_state_conflict(action: &str, status: &str) -> (StatusCode, Json<ErrorResponse>) {
        error_response(
            StatusCode::CONFLICT,
            format!("Cannot {action} run in '{status}' state"),
        )
    }

    fn durable_persist_error(action: &str, error: String) -> (StatusCode, Json<ErrorResponse>) {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Failed to persist durable run {action}: {error}"),
        )
    }

    #[cfg(test)]
    pub(crate) async fn test_llm_cancel_token_is_cancelled(&self, run_id: &str) -> Option<bool> {
        let runs = self.runs.read().await;
        runs.get(run_id).map(|r| r.llm_cancel_token.is_cancelled())
    }

    #[cfg(test)]
    pub(crate) async fn test_pause_flag_is_set(&self, run_id: &str) -> Option<bool> {
        let runs = self.runs.read().await;
        runs.get(run_id)
            .map(|r| r.pause_flag.load(Ordering::Acquire))
    }
}

/// Build an [`ExtractionRequest`] from the current loop state for shutdown-time
/// memory extraction. Returns `None` when no session id is set.
fn build_shutdown_extraction_request(
    state: &AgenticLoopState,
) -> Option<crate::session_memory::ExtractionRequest> {
    state.context_manifest_user_id.as_ref()?;
    state
        .current_session_id
        .as_ref()
        .map(|session_id| crate::session_memory::ExtractionRequest {
            session_id: session_id.clone(),
            messages: state.messages.clone(),
            session_facts: state.session_facts.clone(),
            had_error: state.error_recovery.consecutive_same_error > 0,
            reanchors_current_objective: state
                .turn_intent
                .as_ref()
                .is_some_and(|intent| intent.reanchors_current_objective()),
            turn_number: state.current_session_turn_number(),
        })
}

fn exact_runtime_string(
    field: &'static str,
    value: &str,
    code: &'static str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            format!(
                "{field} must be a non-empty exact string without leading/trailing whitespace or control characters"
            ),
            code,
        ));
    }
    Ok(())
}

fn exact_runtime_id(
    field: &'static str,
    value: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    exact_runtime_string(field, value, "agent_binding_capability_ref_invalid")?;
    if value.contains('/') || value.contains('\\') {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            format!("{field} must not contain path separators"),
            "agent_binding_capability_ref_invalid",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParsedRuntimeBearer<'a> {
    token: &'a str,
}

fn runtime_bearer_token_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.' | '_' | '~' | '+' | '/' | '=')
}

fn parse_runtime_bearer_authorization(
    value: &str,
) -> Result<ParsedRuntimeBearer<'_>, (StatusCode, Json<ErrorResponse>)> {
    exact_runtime_string(
        "runtime_auth.authorization",
        value,
        "agent_binding_runtime_auth_invalid",
    )?;
    let Some(token) = value.strip_prefix("Bearer ") else {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "runtime_auth.authorization must use Bearer scheme",
            "agent_binding_runtime_auth_invalid",
        ));
    };
    if token.is_empty()
        || token.trim() != token
        || token
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
        || !token.chars().all(runtime_bearer_token_char)
    {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "runtime_auth.authorization must contain exactly one Bearer token",
            "agent_binding_runtime_auth_invalid",
        ));
    }
    Ok(ParsedRuntimeBearer { token })
}

fn validate_runtime_authorization(
    runtime_auth: &RuntimeAuthRequest,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let parsed = parse_runtime_bearer_authorization(&runtime_auth.authorization)?;
    debug_assert!(!parsed.token.is_empty());
    Ok(())
}

fn server_workspace_provision_error(
    error: ServerWorkspaceProvisionError,
) -> (StatusCode, Json<ErrorResponse>) {
    match error {
        ServerWorkspaceProvisionError::InvalidSessionId => error_response(
            StatusCode::BAD_REQUEST,
            "Invalid session_id for server workspace provisioning",
        ),
        ServerWorkspaceProvisionError::WorkspaceEscapedBase { .. } => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Server workspace provisioning escaped its base directory: {error}"),
        ),
        _ => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Failed to provision server workspace: {error}"),
        ),
    }
}

fn cloud_workspace_provision_request_from_request(
    request: &ChatRequestData,
    run_id: &str,
) -> Result<Option<RuntimeWorkspaceProvisionRequest>, (StatusCode, Json<ErrorResponse>)> {
    let Some(binding) = request.workspace_binding.as_ref() else {
        return Ok(None);
    };
    match binding.kind {
        astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace => {
            cloud_workspace_provision_request_from_source(binding, run_id).map(Some)
        }
        _ => Ok(None),
    }
}

fn cloud_workspace_provision_request_from_source(
    binding: &astra_services::runs::WorkspaceBindingRequest,
    run_id: &str,
) -> Result<RuntimeWorkspaceProvisionRequest, (StatusCode, Json<ErrorResponse>)> {
    let Some(source) = binding.source.as_ref() else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "Cloud workspace requires an explicit source",
        ));
    };
    match source {
        astra_services::runs::WorkspaceSourceRequest::PersistentVolume { .. } => {
            cloud_persistent_volume_provision_request(binding, run_id)
        }
        astra_services::runs::WorkspaceSourceRequest::UploadedSnapshot { .. } => {
            cloud_uploaded_snapshot_provision_request(binding, run_id)
        }
        astra_services::runs::WorkspaceSourceRequest::GitCheckout { .. } => {
            cloud_git_checkout_provision_request(binding, run_id)
        }
        astra_services::runs::WorkspaceSourceRequest::Scratch => {
            cloud_scratch_workspace_provision_request(binding, run_id)
        }
        astra_services::runs::WorkspaceSourceRequest::Template { .. }
        | astra_services::runs::WorkspaceSourceRequest::DatasetBundle { .. }
        | astra_services::runs::WorkspaceSourceRequest::ArtifactBundle { .. } => {
            cloud_materialized_source_provision_request(binding, run_id)
        }
        astra_services::runs::WorkspaceSourceRequest::EdgePath { .. } => Err(error_response(
            StatusCode::BAD_REQUEST,
            "Cloud workspace source is not supported by this provisioner yet",
        )),
    }
}

fn cloud_persistent_volume_provision_request(
    binding: &astra_services::runs::WorkspaceBindingRequest,
    run_id: &str,
) -> Result<RuntimeWorkspaceProvisionRequest, (StatusCode, Json<ErrorResponse>)> {
    let Some(astra_services::runs::WorkspaceSourceRequest::PersistentVolume { volume_id }) =
        binding.source.as_ref()
    else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "Cloud workspace requires source.kind=persistent_volume",
        ));
    };
    let volume_id = non_empty_request_string(Some(volume_id.as_str())).ok_or_else(|| {
        error_response(
            StatusCode::BAD_REQUEST,
            "Cloud workspace requires a non-empty source.volume_id",
        )
    })?;
    Ok(RuntimeWorkspaceProvisionRequest {
        workspace_id: cloud_workspace_id(run_id),
        owner_scope: RuntimeWorkspaceOwnerScope::Tenant,
        kind: astra_runtime_env::WorkspaceBindingKind::CloudWorkspace,
        authority: runtime_workspace_authority_from_request(
            binding.authority,
            astra_runtime_env::WorkspaceAuthority::ReadWrite,
        ),
        source: RuntimeWorkspaceSource::PersistentVolume { volume_id },
        persistence: RuntimeWorkspacePersistence::Persistent,
        requested_root: non_empty_request_string(binding.root.as_deref()),
        display_name: non_empty_request_string(binding.display_name.as_deref())
            .or_else(|| Some("Cloud workspace".to_string())),
    })
}

fn cloud_uploaded_snapshot_provision_request(
    binding: &astra_services::runs::WorkspaceBindingRequest,
    run_id: &str,
) -> Result<RuntimeWorkspaceProvisionRequest, (StatusCode, Json<ErrorResponse>)> {
    let workspace_id = cloud_workspace_id(run_id);
    let Some(astra_services::runs::WorkspaceSourceRequest::UploadedSnapshot { artifact_id, root }) =
        binding.source.as_ref()
    else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "Uploaded snapshot workspace requires source.kind=uploaded_snapshot",
        ));
    };
    let artifact_id = non_empty_request_string(Some(artifact_id.as_str())).ok_or_else(|| {
        error_response(
            StatusCode::BAD_REQUEST,
            "Uploaded snapshot workspace requires a non-empty source.artifact_id",
        )
    })?;
    let requested_root = non_empty_request_string(root.as_deref())
        .or_else(|| non_empty_request_string(binding.root.as_deref()));
    validate_absolute_materialized_source_root(
        "Uploaded snapshot source.root",
        requested_root.as_deref(),
    )?;
    let authority = runtime_workspace_authority_from_request(
        binding.authority,
        astra_runtime_env::WorkspaceAuthority::ReadOnly,
    );
    Ok(RuntimeWorkspaceProvisionRequest {
        workspace_id,
        owner_scope: RuntimeWorkspaceOwnerScope::Tenant,
        kind: astra_runtime_env::WorkspaceBindingKind::CloudWorkspace,
        authority,
        source: RuntimeWorkspaceSource::UploadedSnapshot { artifact_id },
        persistence: if authority == astra_runtime_env::WorkspaceAuthority::ReadWrite {
            RuntimeWorkspacePersistence::Session
        } else {
            RuntimeWorkspacePersistence::ImmutableSnapshot
        },
        requested_root,
        display_name: non_empty_request_string(binding.display_name.as_deref())
            .or_else(|| Some("Uploaded snapshot".to_string())),
    })
}

fn cloud_materialized_source_provision_request(
    binding: &astra_services::runs::WorkspaceBindingRequest,
    run_id: &str,
) -> Result<RuntimeWorkspaceProvisionRequest, (StatusCode, Json<ErrorResponse>)> {
    let Some(source) = binding.source.as_ref() else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "Materialized cloud workspace requires an explicit source",
        ));
    };
    let (source, default_authority, display_name) = match source {
        astra_services::runs::WorkspaceSourceRequest::Template { template_id } => {
            let template_id =
                non_empty_request_string(Some(template_id.as_str())).ok_or_else(|| {
                    error_response(
                        StatusCode::BAD_REQUEST,
                        "Template workspace requires a non-empty source.template_id",
                    )
                })?;
            (
                RuntimeWorkspaceSource::Template { template_id },
                astra_runtime_env::WorkspaceAuthority::ReadWrite,
                "Template workspace",
            )
        }
        astra_services::runs::WorkspaceSourceRequest::DatasetBundle { dataset_id } => {
            let dataset_id =
                non_empty_request_string(Some(dataset_id.as_str())).ok_or_else(|| {
                    error_response(
                        StatusCode::BAD_REQUEST,
                        "Dataset workspace requires a non-empty source.dataset_id",
                    )
                })?;
            (
                RuntimeWorkspaceSource::DatasetBundle { dataset_id },
                astra_runtime_env::WorkspaceAuthority::ReadOnly,
                "Dataset workspace",
            )
        }
        astra_services::runs::WorkspaceSourceRequest::ArtifactBundle { artifact_id } => {
            let artifact_id =
                non_empty_request_string(Some(artifact_id.as_str())).ok_or_else(|| {
                    error_response(
                        StatusCode::BAD_REQUEST,
                        "Artifact workspace requires a non-empty source.artifact_id",
                    )
                })?;
            (
                RuntimeWorkspaceSource::ArtifactBundle { artifact_id },
                astra_runtime_env::WorkspaceAuthority::ReadOnly,
                "Artifact workspace",
            )
        }
        _ => {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "Materialized cloud workspace requires source.kind=template, dataset_bundle, or artifact_bundle",
            ));
        }
    };
    let requested_root = non_empty_request_string(binding.root.as_deref());
    validate_absolute_materialized_source_root(
        "Cloud workspace source.root",
        requested_root.as_deref(),
    )?;
    let authority = runtime_workspace_authority_from_request(binding.authority, default_authority);
    Ok(RuntimeWorkspaceProvisionRequest {
        workspace_id: cloud_workspace_id(run_id),
        owner_scope: RuntimeWorkspaceOwnerScope::Tenant,
        kind: astra_runtime_env::WorkspaceBindingKind::CloudWorkspace,
        authority,
        source,
        persistence: if authority == astra_runtime_env::WorkspaceAuthority::ReadWrite {
            RuntimeWorkspacePersistence::Session
        } else {
            RuntimeWorkspacePersistence::ImmutableSnapshot
        },
        requested_root,
        display_name: non_empty_request_string(binding.display_name.as_deref())
            .or_else(|| Some(display_name.to_string())),
    })
}

fn cloud_git_checkout_provision_request(
    binding: &astra_services::runs::WorkspaceBindingRequest,
    run_id: &str,
) -> Result<RuntimeWorkspaceProvisionRequest, (StatusCode, Json<ErrorResponse>)> {
    let Some(astra_services::runs::WorkspaceSourceRequest::GitCheckout {
        repository,
        reference,
    }) = binding.source.as_ref()
    else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "Git checkout workspace requires source.kind=git_checkout",
        ));
    };
    let repository = non_empty_request_string(Some(repository.as_str())).ok_or_else(|| {
        error_response(
            StatusCode::BAD_REQUEST,
            "Git checkout workspace requires a non-empty source.repository",
        )
    })?;
    Ok(RuntimeWorkspaceProvisionRequest {
        workspace_id: cloud_workspace_id(run_id),
        owner_scope: RuntimeWorkspaceOwnerScope::Tenant,
        kind: astra_runtime_env::WorkspaceBindingKind::CloudWorkspace,
        authority: runtime_workspace_authority_from_request(
            binding.authority,
            astra_runtime_env::WorkspaceAuthority::ReadWrite,
        ),
        source: RuntimeWorkspaceSource::GitCheckout {
            repository,
            reference: non_empty_request_string(reference.as_deref()),
        },
        persistence: RuntimeWorkspacePersistence::Session,
        requested_root: None,
        display_name: non_empty_request_string(binding.display_name.as_deref())
            .or_else(|| Some("Git checkout".to_string())),
    })
}

fn cloud_scratch_workspace_provision_request(
    binding: &astra_services::runs::WorkspaceBindingRequest,
    run_id: &str,
) -> Result<RuntimeWorkspaceProvisionRequest, (StatusCode, Json<ErrorResponse>)> {
    if !matches!(
        binding.source.as_ref(),
        Some(astra_services::runs::WorkspaceSourceRequest::Scratch)
    ) {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "Scratch cloud workspace requires source.kind=scratch",
        ));
    }
    Ok(RuntimeWorkspaceProvisionRequest {
        workspace_id: cloud_workspace_id(run_id),
        owner_scope: RuntimeWorkspaceOwnerScope::Tenant,
        kind: astra_runtime_env::WorkspaceBindingKind::CloudWorkspace,
        authority: runtime_workspace_authority_from_request(
            binding.authority,
            astra_runtime_env::WorkspaceAuthority::ReadWrite,
        ),
        source: RuntimeWorkspaceSource::Scratch,
        persistence: RuntimeWorkspacePersistence::Session,
        requested_root: non_empty_request_string(binding.root.as_deref()),
        display_name: non_empty_request_string(binding.display_name.as_deref())
            .or_else(|| Some("Scratch workspace".to_string())),
    })
}

fn cloud_workspace_id(run_id: &str) -> String {
    format!("run-{run_id}")
}

fn non_empty_request_string(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn validate_absolute_materialized_source_root(
    label: &str,
    root: Option<&str>,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if root.is_some_and(|root| !Path::new(root).is_absolute()) {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("{label} must be an absolute materialized source path"),
        ));
    }
    Ok(())
}

fn runtime_workspace_authority_from_request(
    authority: Option<astra_services::runs::WorkspaceAuthorityRequest>,
    default: astra_runtime_env::WorkspaceAuthority,
) -> astra_runtime_env::WorkspaceAuthority {
    match authority {
        Some(astra_services::runs::WorkspaceAuthorityRequest::ReadOnly) => {
            astra_runtime_env::WorkspaceAuthority::ReadOnly
        }
        Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite) => {
            astra_runtime_env::WorkspaceAuthority::ReadWrite
        }
        Some(astra_services::runs::WorkspaceAuthorityRequest::None) => {
            astra_runtime_env::WorkspaceAuthority::None
        }
        None => default,
    }
}

fn execution_bindings_from_workspace_record(
    record: &RuntimeWorkspaceRecord,
) -> ExecutionBindingSnapshot {
    let workspace = server_workspace_binding_from_workspace_record(record);
    let executor = ExecutorBinding::orchestrator_managed(
        format!("orchestrator:{}", record.workspace_id),
        "Orchestrator-managed executor",
        ExecutorStatus::Online,
    );
    let runtime = astra_runtime_env::RuntimeBinding::kubernetes(format!(
        "kubernetes:{}",
        record.workspace_id
    ));
    ExecutionBindingSnapshot::new(workspace, executor, runtime)
}

fn server_workspace_binding_from_workspace_record(
    record: &RuntimeWorkspaceRecord,
) -> WorkspaceBinding {
    WorkspaceBinding {
        kind: match record.kind {
            astra_runtime_env::WorkspaceBindingKind::LocalFilesystem => {
                WorkspaceBindingKind::Unknown
            }
            other => other,
        },
        display_name: record.display_name.clone(),
        cwd: if record.kind == astra_runtime_env::WorkspaceBindingKind::None {
            None
        } else {
            Some(record.root_or_volume_ref.clone())
        },
        authority: record.authority,
    }
}

fn cloud_workspace_provision_error(
    error: RuntimeWorkspaceProvisionError,
) -> (StatusCode, Json<ErrorResponse>) {
    match error.kind {
        RuntimeWorkspaceProvisionErrorKind::InvalidWorkspaceId
        | RuntimeWorkspaceProvisionErrorKind::SourceKindMismatch
        | RuntimeWorkspaceProvisionErrorKind::AuthorityDenied => error_response(
            StatusCode::BAD_REQUEST,
            format!("Invalid cloud workspace request: {error}"),
        ),
        RuntimeWorkspaceProvisionErrorKind::MountFailed
        | RuntimeWorkspaceProvisionErrorKind::Internal
        | RuntimeWorkspaceProvisionErrorKind::CleanupFailed => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to provision cloud workspace: {error}"),
        ),
        RuntimeWorkspaceProvisionErrorKind::WorkspaceUnavailable => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Cloud workspace unavailable: {error}"),
        ),
    }
}

/// Returns the effective WorkspaceRecord for tool execution:
/// - If a cloud workspace record is present, use it (standard path).
/// - Otherwise, if the request carries a provider_workspace_id (from the
///   edge-registration token's provider_scope_id on MOI provider-authorized
///   turns), synthesise a minimal WorkspaceRecord so that edge workspace
///   isolation checks in the transport layer work correctly.
fn provider_effective_workspace_record(
    cloud: Option<&astra_runtime_env::WorkspaceRecord>,
    provider_workspace_id: Option<&str>,
) -> Option<astra_runtime_env::WorkspaceRecord> {
    cloud.cloned().or_else(|| {
        provider_workspace_id.map(|ws_id| astra_runtime_env::WorkspaceRecord {
            workspace_id: ws_id.to_string(),
            owner_scope: astra_runtime_env::WorkspaceOwnerScope::None,
            kind: astra_runtime_env::WorkspaceBindingKind::None,
            authority: astra_runtime_env::WorkspaceAuthority::None,
            root_or_volume_ref: String::new(),
            source: astra_runtime_env::WorkspaceSource::None,
            persistence: astra_runtime_env::WorkspacePersistence::None,
            revision: String::new(),
            display_name: String::new(),
        })
    })
}

fn workspace_record_store_error(
    error: WorkspaceRecordStoreError,
) -> (StatusCode, Json<ErrorResponse>) {
    match error {
        WorkspaceRecordStoreError::InvalidOwnerId
        | WorkspaceRecordStoreError::InvalidSessionId
        | WorkspaceRecordStoreError::InvalidRunId
        | WorkspaceRecordStoreError::InvalidWorkspaceId(_) => error_response(
            StatusCode::BAD_REQUEST,
            format!("Invalid workspace ownership record: {error}"),
        ),
        WorkspaceRecordStoreError::WorkspaceOwnerConflict { .. }
        | WorkspaceRecordStoreError::SourceOwnerConflict { .. } => error_response(
            StatusCode::CONFLICT,
            format!("Workspace ownership conflict: {error}"),
        ),
        WorkspaceRecordStoreError::Database(_)
        | WorkspaceRecordStoreError::Json(_)
        | WorkspaceRecordStoreError::Unavailable(_) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Failed to persist workspace record: {error}"),
        ),
    }
}
#[async_trait]
impl RunLifecycleService for AgenticRunLifecycleService {
    /// Create a run (background mode): spawns the agentic loop in a task, returns immediately.
    async fn create_run(
        &self,
        user_id: String,
        request: ChatRequestData,
    ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)> {
        let request = self.prepare_chat_request(request).await?;
        let request_constraints = self
            .validate_request_constraints(&user_id, &request)
            .await?;
        let mut request = request;
        Self::install_agent_binding_runtime_forward_headers(&mut request)?;

        // ── Resource governance check (Phase 5) ─────────────────────
        if let Some(ref gov) = self.resource_governor {
            if let astra_services::resource_governor::LimitCheck::Denied { limit, reason } =
                gov.check_run_start(&user_id).await
            {
                return Err(per_user_run_quota_response(limit, reason));
            }
        }

        let run_id = Uuid::new_v4().to_string();
        let session_id = request
            .session_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let agent_binding_mode = request.agent_binding.is_some();
        let edge_context = Self::extract_edge_context(&request)?;
        let edge_tools = edge_context.edge_tools.clone();
        let server_service_tool_catalog_enabled =
            Self::server_service_tool_catalog_enabled_for_request(
                agent_binding_mode,
                edge_context.has_tools(),
            );
        let runtime_capabilities = self
            .prepare_runtime_capabilities(&request, &request_constraints)
            .await?;
        let requires_runtime_mcp_executor = runtime_capabilities.mcp_bundle.is_some();
        let mut edge_profile = edge_context.edge_profile.to_map();
        Self::apply_agent_binding_prompt_context(
            &mut edge_profile,
            runtime_capabilities.agent_binding.as_ref(),
            request.runtime_system_prompt.as_deref(),
            request.context.as_ref(),
        );

        // Guard: reject if this session already has a blocking run.
        // Hold write lock across check+insert to prevent TOCTOU race.
        let (run_state, cancel_flag, pause_flag, llm_cancel_token) =
            Self::build_tracked_run_state(run_id.clone(), session_id.clone(), user_id.clone());
        {
            let mut runs = self.runs.write().await;
            let has_active = Self::session_has_blocking_run(&runs, &session_id);
            if has_active {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "session already has an active run".to_string(),
                ));
            }
            runs.insert(run_id.clone(), run_state);
        }

        // Provision explicit workspace bindings early so build_initial_state
        // and durable run_started metadata use the same execution boundary.
        let cloud_workspace_record = match self
            .provision_cloud_workspace_record(&user_id, &session_id, &request, &run_id)
            .await
        {
            Ok(record) => record,
            Err(error) => {
                self.runs.write().await.remove(&run_id);
                return Err(error);
            }
        };
        let cloud_execution_bindings = cloud_workspace_record
            .as_ref()
            .map(|record| execution_bindings_from_workspace_record(record));
        let cloud_workspace = cloud_workspace_record
            .as_ref()
            .map(|record| PathBuf::from(&record.root_or_volume_ref));

        let server_workspace =
            if cloud_workspace_record.is_none() && request_uses_server_workspace(&request) {
                match self.provision_server_workspace(&session_id) {
                    Ok(workspace) => Some(workspace),
                    Err(error) => {
                        self.runs.write().await.remove(&run_id);
                        return Err(error);
                    }
                }
            } else {
                None
            };
        let execution_bindings = cloud_execution_bindings
            .or_else(|| {
                server_workspace.as_deref().map(|workspace| {
                    let (workspace, executor) =
                        resolve_request_execution_bindings(&request, workspace);
                    ExecutionBindingSnapshot::inferred(workspace, executor)
                })
            })
            .or_else(|| {
                resolve_request_execution_bindings_without_server_workspace(&request, &edge_profile)
                    .map(|(workspace, executor)| {
                        ExecutionBindingSnapshot::inferred(workspace, executor)
                    })
            });
        if let Err(error) = self
            .validate_optional_tool_availability(
                &user_id,
                &request_constraints,
                execution_bindings.as_ref(),
            )
            .await
        {
            self.runs.write().await.remove(&run_id);
            if let Some(record) = cloud_workspace_record.as_ref() {
                self.cleanup_cloud_workspace_after_failed_start(
                    &user_id,
                    &session_id,
                    &run_id,
                    record,
                    "enabled optional tools became unavailable before run start".to_string(),
                )
                .await;
            }
            return Err(error);
        }
        let tool_runtime_workspace = cloud_workspace.clone().or_else(|| server_workspace.clone());
        let server_tool_executor_workspace = if let Some(workspace) = tool_runtime_workspace.clone()
        {
            Some(workspace)
        } else if !agent_binding_mode || requires_runtime_mcp_executor {
            match self.provision_server_workspace(&session_id) {
                Ok(workspace) => Some(workspace),
                Err(error) => {
                    self.runs.write().await.remove(&run_id);
                    return Err(error);
                }
            }
        } else {
            None
        };

        if let Err(error) = self
            .persist_run_start(
                &run_id,
                &user_id,
                &session_id,
                &request,
                execution_bindings.as_ref(),
                runtime_capabilities.agent_binding.as_ref(),
            )
            .await
        {
            self.runs.write().await.remove(&run_id);
            if let Some(record) = cloud_workspace_record.as_ref() {
                self.cleanup_cloud_workspace_after_failed_start(
                    &user_id,
                    &session_id,
                    &run_id,
                    record,
                    format!(
                        "durable run start failed after cloud workspace provisioning: {}",
                        error.1.0.detail
                    ),
                )
                .await;
            }
            return Err(error);
        }

        // Spawn background agentic loop.
        // Load plan state as structured data: prompt hint for context, plus
        // an independent authoring flag for the tool gate. Ordinary session
        // resume context must not activate plan-mode blocking.
        let plan_resume_snapshot = if let Some(shared) = &self.shared_pool {
            let repo = astra_plan::CloudPlanRepository::new(shared.get().clone());
            astra_plan::plan_resume_snapshot_for_session(&repo, &user_id, &session_id).await
        } else {
            astra_plan::PlanResumeSnapshot::default()
        };
        let restore_prior_prompt_history = should_restore_prior_prompt_history(
            request.session_id.is_some(),
            self.session_has_prior_prompt_history(&user_id, &session_id)
                .await,
        );
        let session_resume_hint = self
            .session_resume_hydration_hint_for_session(
                &user_id,
                &session_id,
                &run_id,
                restore_prior_prompt_history,
            )
            .await;
        let plan_resume_hint = astra_turn_core::resume_hydration::merge_resume_hints(
            session_resume_hint,
            plan_resume_snapshot.prompt_hint,
        );
        let plan_authoring_active = plan_resume_snapshot.authoring_active;
        let task_board_resume_hint = self
            .task_board_resume_hint_for_session(&user_id, &session_id)
            .await;
        let mut host = self.build_host(
            &user_id,
            &session_id,
            &run_id,
            &request,
            edge_tools,
            edge_profile.clone(),
            server_service_tool_catalog_enabled,
            !agent_binding_mode,
            execution_bindings.as_ref(),
            plan_resume_hint,
            plan_authoring_active,
            task_board_resume_hint,
        );
        let interaction_sink: Arc<dyn server_loop_host::HostInteractionSink> =
            Arc::new(DurableHostInteractionSink {
                run_engine: self.run_engine.clone(),
                user_id: user_id.clone(),
                run_id: run_id.clone(),
                session_id: session_id.clone(),
                agent_id: None,
                event_tx: None,
            });
        host.set_interaction_sink(Arc::clone(&interaction_sink));
        if let Some(snapshot) = execution_bindings.as_ref() {
            host.set_execution_metadata(Value::Object(binding_event_fields(
                &snapshot.workspace,
                &snapshot.executor,
            )));
        }
        if let Some(ref bundle) = runtime_capabilities.mcp_bundle {
            host.install_runtime_tool_schemas(bundle.schemas.clone(), bundle.control_tools.clone());
            host.install_runtime_stop_after_success_tools(bundle.stop_after_success_tools.clone());
        }
        // In agent-binding mode with an EdgeAgent executor, the host only installs
        // MCP schemas by default. Merge edge-builtin tool schemas (bash, read_file, …)
        // for any tools explicitly listed in allow_tools so the model can see and call
        // them via the edge dispatch path.
        if let Some(snapshot) = execution_bindings.as_ref() {
            if snapshot.executor.kind == ExecutorBindingKind::EdgeAgent {
                if let Some(allow_tools) = request.allow_tools.as_deref() {
                    let tools: Vec<String> = allow_tools.iter().map(|s| s.to_string()).collect();
                    host.merge_allowlisted_edge_tool_schemas(&tools);
                }
            }
        }
        let mut loop_state = self.build_initial_state_inner(
            &user_id,
            &request,
            &session_id,
            &run_id,
            tool_runtime_workspace
                .as_deref()
                .or(server_workspace.as_deref()),
            execution_bindings.as_ref(),
            Some(llm_cancel_token.clone()),
            Some(Arc::clone(&interaction_sink)),
            request_constraints.clone(),
            &edge_context,
            Some(&edge_profile),
            runtime_capabilities.request_scoped_skill_resolver.clone(),
            runtime_capabilities.agent_binding.as_ref(),
        );
        loop_state.context_manifest_user_id = Some(user_id.clone());
        loop_state.runtime_manifest = Self::build_runtime_manifest(
            &request,
            &runtime_capabilities,
            tool_runtime_workspace.is_some(),
        );
        // Inject user_id into the harness sink used by DB-persistence tests.
        #[cfg(feature = "harness")]
        loop_state.harness.set_user_id(&user_id);

        loop_state.session_turn =
            infer_session_turn(self.shared_pool.as_ref(), &user_id, &session_id).await;
        self.configure_host_approval_audit_context(
            &mut host,
            &user_id,
            &session_id,
            &run_id,
            loop_state.session_turn,
            request.agent_id.clone(),
        );
        let fresh_session_current_date = loop_state
            .pipeline_session
            .as_ref()
            .map(|session| session.current_date().to_string())
            .unwrap_or_else(|| {
                crate::turn::session_current_date::resolve_session_current_date(&session_id)
            });

        // ── Runtime warm-start: restore loop state from checkpoint ──
        // Overwrites fresh advisory state with checkpointed pipeline,
        // compaction, and context-window counters. Without this, server-side
        // session resume starts cold even though finalization persisted the
        // state needed for long-running sessions.
        if restore_prior_prompt_history {
            if let Ok(Some(restored)) =
                astra_pipeline::step_restore::restore_session(&user_id, &session_id)
            {
                restore_step_checkpoint_runtime_state(
                    restored,
                    &fresh_session_current_date,
                    &mut loop_state,
                );
            }
        }

        // ── CSL: Load conversation history from the log ─────────────
        let csl_manager = if restore_prior_prompt_history {
            self.restore_csl_history(&user_id, &session_id, &run_id, &mut loop_state)
                .await
        } else {
            None
        };

        self.configure_loop_state_runtime_controls(
            &mut loop_state,
            &cancel_flag,
            &pause_flag,
            (*llm_cancel_token).clone(),
        );
        configure_runtime_controllers(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &mut loop_state,
            &user_id,
            &session_id,
        )
        .await;
        // The RuntimeToolExecutor is the owner for server-side runtime tools
        // such as `agent`. For edge-bound runs this workspace is only an
        // internal runtime scratch dir; execution routing still follows the
        // explicit workspace/executor binding and cannot silently fall back.
        if let Some(workspace) = server_tool_executor_workspace {
            let memoria_base = Some(astra_core::MemoriaSettings::from_env().base_url);
            let task_store = match astra_tools::task_mgmt_matrixone::select_task_store(
                self.shared_pool.as_ref().map(|p| p.get().clone()),
                user_id.clone(),
            ) {
                Ok(store) => store,
                Err(error) => {
                    let message =
                        format!("tool executor setup failed after durable run start: {error}");
                    self.fail_started_run_before_spawn(
                        &user_id,
                        &run_id,
                        &message,
                        PreSpawnFailureCode::PreSpawnFailure,
                    )
                    .await;
                    if let Some(record) = cloud_workspace_record.as_ref() {
                        self.cleanup_cloud_workspace_after_failed_start(
                            &user_id,
                            &session_id,
                            &run_id,
                            record,
                            message,
                        )
                        .await;
                    }
                    return Err(error_response(StatusCode::INTERNAL_SERVER_ERROR, error));
                }
            };
            let mut executor = runtime_tool_executor::RuntimeToolExecutor::new(
                workspace.clone(),
                user_id.clone(),
                session_id.clone(),
                memoria_base,
                None,
            );
            executor = wire_reflect_service_into_executor(executor, &self.reflect_service)
                .with_cancel_token(loop_state.cancellation.token.clone())
                .with_task_store(task_store);
            if agent_binding_mode {
                executor = executor.with_server_service_tools_disabled();
            } else {
                executor =
                    executor.with_capabilities(crate::capabilities::lifecycle_server_capabilities(
                        self.shared_pool.is_some(),
                        self.reflect_service.is_configured(),
                    ));
            }

            // Apply shared ToolExecutionService (with admin-controllable disabled_tool_offers)
            // or fall back to building one from deployment config.
            if let Some(ref shared_tes) = self.tool_execution_service {
                executor = executor.with_tool_execution_service(shared_tes.clone());
            } else {
                let mut builder = apply_deployment_tool_policy(
                    ToolExecutionService::builder(),
                    &load_deployment_tool_policy(),
                );
                if let Some(pool) = &self.edge_connection_pool {
                    builder = builder.edge_connection_pool(pool.clone());
                }
                if let Some(svc) = &self.edge_dispatch_service {
                    builder = builder.edge_dispatch_service(Arc::clone(svc));
                }
                if let Some(svc) = &self.edge_registry_service {
                    builder = builder.edge_registry_service(Arc::clone(svc));
                }
                executor = executor.with_tool_execution_service(builder.build());
            }

            if let Some(ref bundle) = runtime_capabilities.mcp_bundle {
                if let Some(manager) = &bundle.manager {
                    executor.set_mcp_manager(manager.clone());
                }
                if let Some(agent_binding_mcp) = &bundle.agent_binding_mcp {
                    executor.set_agent_binding_mcp(agent_binding_mcp.clone());
                }
                executor.set_request_scoped_mcp_schemas(bundle.schemas.clone());
                executor.set_provider_policy_index(bundle.provider_policy_index.clone());
            }
            // Wire the plan repository so enter/exit_plan_mode tools work and
            // the write-tool guard can check `active_plan_id`.
            if let Some(shared) = &self.shared_pool {
                executor.set_context_manifest_pool(shared.clone());
                executor = executor.with_session_artifact_store(
                    astra_services::DatabaseSessionArtifactStore::new(self.matrixone.clone())
                        .with_pool(shared.clone()),
                );
                executor.set_plan_repository(std::sync::Arc::new(
                    astra_plan::CloudPlanRepository::new(shared.get().clone()),
                ));
            }
            // Select the database ledger only after composition has attached
            // its shared pool; offline composition selects the in-memory
            // adapter explicitly.
            executor.enable_durable_invocations();
            configure_runtime_semantic_read_cache(
                &mut executor,
                runtime_capabilities.mcp_bundle.as_ref(),
            );
            // Share the host's plan-resume hint slot so tool-triggered
            // plan-mode changes refresh the system prompt mid-run.
            executor.set_plan_resume_hint_handle(host.plan_resume_hint_handle());
            executor.set_plan_authoring_active_handle(host.plan_authoring_active_handle());
            if let Some(observability_session) = loop_state.telemetry.observability_session.clone()
            {
                executor.set_observability_session(observability_session);
            }
            if let Some(writer) = self.auxiliary_event_writer.clone() {
                executor.set_auxiliary_event_writer(writer);
            }
            let binding_snapshot = execution_bindings.clone().unwrap_or_else(|| {
                let (workspace_binding, executor_binding) =
                    resolve_request_execution_bindings(&request, workspace.as_path());
                ExecutionBindingSnapshot::inferred(workspace_binding, executor_binding)
            });
            let agent_working_dir =
                agent_working_dir_for_bindings(execution_bindings.as_ref(), workspace.as_path());
            executor.set_execution_binding_snapshot(binding_snapshot);
            executor.set_workspace_record(provider_effective_workspace_record(
                cloud_workspace_record.as_ref(),
                request.provider_workspace_id.as_deref(),
            ));
            host.set_execution_metadata(executor.binding_metadata());

            let active_work_registry = self
                .wire_server_dynamic_agent_tools(
                    &mut executor,
                    &user_id,
                    &session_id,
                    &run_id,
                    loop_state.session_turn,
                    &request,
                    agent_working_dir.as_path(),
                    None,
                    None,
                    Some(pause_flag.clone()),
                    Some(llm_cancel_token.clone()),
                    #[cfg(feature = "harness")]
                    loop_state.harness.sink.clone(),
                )
                .await;
            loop_state.attach_active_work_registry(active_work_registry);

            if request.interactive_client {
                // ── Phase E: Wire WebSocket approval and ask_user gates ───
                let (approval_tx, approval_rx) = mpsc::channel::<Value>(64);
                let approval_gate = DurableRunApprovalGate::new(
                    user_id.clone(),
                    session_id.clone(),
                    run_id.clone(),
                    Some(loop_state.session_turn),
                    self.run_engine.clone(),
                    self.runs_handle(),
                    Some(approval_tx),
                    None,
                )
                .with_cancel_token(llm_cancel_token.clone());
                executor.set_approval_gate(std::sync::Arc::new(approval_gate));
                self.approval_channels
                    .lock()
                    .await
                    .insert(run_id.clone(), approval_rx);

                let (user_prompt_tx, user_prompt_rx) = mpsc::channel::<Value>(64);
                let user_prompt_gate = DurableRunUserPromptGate::new(
                    user_id.clone(),
                    session_id.clone(),
                    run_id.clone(),
                    Some(loop_state.session_turn),
                    self.run_engine.clone(),
                    self.runs_handle(),
                    Some(user_prompt_tx),
                    None,
                )
                .with_cancel_token(llm_cancel_token.clone());
                executor.set_ask_user_gate(std::sync::Arc::new(user_prompt_gate));
                self.user_prompt_channels
                    .lock()
                    .await
                    .insert(run_id.clone(), user_prompt_rx);

                // ── Phase F.3: Wire WebSocket progress callback ──────
                let (progress_tx, progress_rx) = mpsc::channel::<ProgressEvent>(64);
                let progress_cb =
                    astra_server_types::ws_progress_callback::WebSocketProgressCallback::new(
                        progress_tx,
                    );
                executor.set_progress_callback(std::sync::Arc::new(progress_cb));
                self.progress_channels
                    .lock()
                    .await
                    .insert(run_id.clone(), progress_rx);
            } else {
                executor.set_approval_gate(std::sync::Arc::new(
                    DurableRunApprovalGate::new(
                        user_id.clone(),
                        session_id.clone(),
                        run_id.clone(),
                        Some(loop_state.session_turn),
                        self.run_engine.clone(),
                        self.runs_handle(),
                        None,
                        None,
                    )
                    .with_cancel_token(llm_cancel_token.clone()),
                ));
                executor.set_ask_user_gate(std::sync::Arc::new(
                    DurableRunUserPromptGate::new(
                        user_id.clone(),
                        session_id.clone(),
                        run_id.clone(),
                        Some(loop_state.session_turn),
                        self.run_engine.clone(),
                        self.runs_handle(),
                        None,
                        None,
                    )
                    .with_cancel_token(llm_cancel_token.clone()),
                ));
            }

            wire_executor_into_state(executor, &mut loop_state);
        }

        // Clone handles we need inside the spawned task.
        let bg_approval_channels = self.approval_channels.clone();
        let bg_user_prompt_channels = self.user_prompt_channels.clone();
        let bg_progress_channels = self.progress_channels.clone();
        let runs = self.runs_handle();
        let run_engine = self.run_engine.clone();
        let bg_run_id = run_id.clone();
        let bg_session_id = session_id.clone();
        let bg_resource_governor = self.resource_governor.clone();
        let bg_user_id = user_id.clone();
        let bg_cloud_workspace_record = cloud_workspace_record.clone();
        let bg_workspace_record_store = self.workspace_record_store.clone();
        let bg_metrics_registry = self.metrics_registry.clone();
        let bg_cancel_flag = cancel_flag.clone();
        let bg_pause_flag = pause_flag.clone();
        let bg_llm_cancel_token = llm_cancel_token.clone();
        let bg_root_mailbox_router = Arc::clone(&self.server_agent_mailbox_router);
        let bg_root_mailbox_agent_id = request
            .agent_id
            .clone()
            .unwrap_or_else(|| "root-agent".to_string());
        let persist_ctx = PostLoopPersistContext {
            matrixone: self.matrixone.clone(),
            shared_pool: self.shared_pool.clone(),
            user_id: user_id.clone(),
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            agent_id: request.agent_id.clone(),
            model_name: request.model.clone(),
            user_message: request.message.clone(),
            hook_db_writer: self.hook_db_writer.clone(),
            observer_worker: self.observer_worker.clone(),
            tool_event_writer: self.tool_event_writer.clone(),
            metrics_registry: self.metrics_registry.clone(),
            csl_manager: csl_manager.map(tokio::sync::Mutex::new),
        };

        // ── Global admission control: limit concurrent agentic loop tasks ──
        // Wait for a configured interval before returning a structured 503.
        let permit = match self.acquire_run_permit(run_admission_timeout()).await {
            Ok(permit) => permit,
            Err(error) => {
                let failure_reason = match error {
                    RunAdmissionError::Timeout => {
                        "server capacity timeout before agentic loop start"
                    }
                    RunAdmissionError::Closed => {
                        "server capacity admission closed before agentic loop start"
                    }
                };
                self.fail_started_run_before_spawn(&user_id, &run_id, failure_reason, error.into())
                    .await;
                self.remove_run_channels(&run_id).await;
                if let Some(record) = cloud_workspace_record.as_ref() {
                    self.cleanup_cloud_workspace_after_failed_start(
                        &user_id,
                        &session_id,
                        &run_id,
                        record,
                        failure_reason.to_string(),
                    )
                    .await;
                }
                return Err(run_admission_capacity_response(error));
            }
        };

        // Background task tracking: background_task_count is incremented before
        // spawn and decremented via RAII guard on exit. serve()'s shutdown path
        // calls drain_background_tasks() to wait for in-flight runs.
        let bg_task_count_1 = Arc::clone(&self.background_task_count);
        bg_task_count_1.fetch_add(1, Ordering::Release);
        spawn_observed(
            async move {
                let _permit = permit; // RAII: released when this task completes
                // RAII guard: decrement counter when this task exits (normal or panic).
                struct TaskCountGuard(Arc<AtomicUsize>);
                impl Drop for TaskCountGuard {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::Release);
                    }
                }
                let _guard = TaskCountGuard(bg_task_count_1);
                let _owner_lease_heartbeat =
                    run_engine.start_owner_lease_heartbeat(bg_user_id.clone(), bg_run_id.clone());

                // Pre-flight: check daily token budget before starting the agentic loop.
                if let Some(ref gov) = bg_resource_governor {
                    use astra_services::resource_governor::LimitCheck;
                    if let LimitCheck::Denied { limit, reason } =
                        gov.check_token_budget(&bg_user_id).await
                    {
                        tracing::warn!(
                            target: "astra_runtime::run_lifecycle",
                            user_id = %bg_user_id,
                            run_id = %bg_run_id,
                            limit = %limit.as_str(),
                            reason = %reason,
                            "run rejected: daily token budget exhausted"
                        );
                        let budget_reject_committed = Self::persist_started_run_quota_rejection(
                            &run_engine,
                            &runs,
                            &bg_user_id,
                            &bg_run_id,
                            limit,
                            &reason,
                        )
                        .await
                        .is_some();
                        // Clean up channels for this run.
                        bg_approval_channels.lock().await.remove(&bg_run_id);
                        bg_user_prompt_channels.lock().await.remove(&bg_run_id);
                        bg_progress_channels.lock().await.remove(&bg_run_id);
                        if budget_reject_committed
                            && let Some(record) = bg_cloud_workspace_record.as_ref()
                        {
                            Self::cleanup_cloud_workspace_with_debt(
                            bg_workspace_record_store.clone(),
                            &bg_user_id,
                            &bg_session_id,
                            &bg_run_id,
                            record,
                            RuntimeCleanupReason::Failed,
                            format!(
                                "run rejected before agentic loop start: daily token budget exhausted: {reason}"
                            ),
                        )
                        .await;
                        }
                        if budget_reject_committed {
                            Self::schedule_run_eviction(&runs, bg_run_id.clone());
                        }
                        return;
                    }
                }
                install_server_root_mailbox(
                    &mut loop_state,
                    &bg_root_mailbox_router,
                    &bg_session_id,
                    &bg_run_id,
                    &bg_root_mailbox_agent_id,
                )
                .await;
                let _control_watcher = start_active_run_control_watcher(
                    loop_state.run_control.clone(),
                    bg_user_id.clone(),
                    bg_run_id.clone(),
                    bg_cancel_flag.clone(),
                    bg_pause_flag.clone(),
                    bg_llm_cancel_token.clone(),
                );

                let outcome =
                    run_agentic_loop_with_host_panic_safe(&mut host, &mut loop_state).await;
                park_server_root_mailbox(&mut loop_state).await;
                let (outcome, events) = host.settle_loop_turn(outcome);
                let loop_success = outcome.is_ok();
                let (events, final_status, error_msg) =
                    Self::finalize_run_events(outcome, events, &loop_state);

                // Clean up channels for this run.
                bg_approval_channels.lock().await.remove(&bg_run_id);
                bg_user_prompt_channels.lock().await.remove(&bg_run_id);
                bg_progress_channels.lock().await.remove(&bg_run_id);
                let terminal_events = enforce_durable_run_event_batch_budget(
                    terminal_events_for_persistence(&events),
                );
                record_durable_run_event_batch_metrics(
                    bg_metrics_registry.as_ref(),
                    "non_streaming_terminal",
                    "planned",
                    &terminal_events,
                );

                // Publish terminal run state before best-effort post-run side effects
                // so background observers do not stay stuck in "running" because a
                // hook, event write, or learning save is slow.
                let mut persisted_status = final_status;
                let mut persist_status_update = true;
                let mut persist_terminal_events = true;

                if let Some(run) = runs.write().await.get_mut(&bg_run_id) {
                    run.execution_live = false;
                    if run.status == RunStatus::Cancelled {
                        persist_status_update = false;
                        persist_terminal_events = false;
                        merge_cancelled_run_events(run, events);
                        if final_status != RunStatus::Waiting {
                            run.live_tx = None;
                        }
                        flush_turn_observability(&mut loop_state, &bg_session_id, true);
                    } else {
                        run.events.extend(events);
                        if should_preserve_manual_pause_on_completion(&run.status, &final_status) {
                            persist_status_update = false;
                            persisted_status = RunStatus::Paused;
                            run.waiting_for
                                .get_or_insert_with(|| "user_resume".to_string());
                            run.live_tx = None;
                        } else if run.status.try_transition(&final_status).is_ok() {
                            run.status = final_status;
                        }
                        if !run.status.is_resumable() {
                            run.live_tx = None;
                        }
                    }
                }

                if persist_status_update
                    && should_preserve_manual_pause_from_durable(
                        &run_engine,
                        &bg_user_id,
                        &bg_run_id,
                        &final_status,
                    )
                    .await
                {
                    persist_status_update = false;
                    persisted_status = RunStatus::Paused;
                    if let Some(run) = runs.write().await.get_mut(&bg_run_id) {
                        if run.status == RunStatus::Paused
                            || run.status.try_transition(&RunStatus::Paused).is_ok()
                        {
                            run.status = RunStatus::Paused;
                            run.pause_flag.store(true, Ordering::SeqCst);
                            run.waiting_for
                                .get_or_insert_with(|| "user_resume".to_string());
                            run.live_tx = None;
                        } else {
                            tracing::warn!(
                                target: "astra_runtime::run_lifecycle",
                                run_id = %bg_run_id,
                                current_status = %run.status.as_str(),
                                "durable pause projection arrived after an incompatible local terminal state"
                            );
                        }
                    }
                }

                let mut durable_status_committed = !persist_status_update;
                let mut terminal_events_committed = false;
                if !persist_terminal_events {
                    record_durable_run_event_batch_metrics(
                        bg_metrics_registry.as_ref(),
                        "non_streaming_terminal",
                        "skipped",
                        &terminal_events,
                    );
                }
                if persist_status_update {
                    let events_for_transition: &[Value] = if persist_terminal_events {
                        terminal_events.as_slice()
                    } else {
                        &[]
                    };
                    match run_engine
                        .commit_terminal_status_with_events_if_current(
                            &bg_user_id,
                            &bg_run_id,
                            &[STATUS_RUNNING, STATUS_WAITING],
                            persisted_status.as_str(),
                            None,
                            error_msg.as_deref(),
                            events_for_transition,
                        )
                        .await
                    {
                        Ok(TerminalTransitionOutcome::Committed) => {
                            durable_status_committed = true;
                            terminal_events_committed =
                                persist_terminal_events && !terminal_events.is_empty();
                            if terminal_events_committed {
                                record_durable_run_event_batch_metrics(
                                    bg_metrics_registry.as_ref(),
                                    "non_streaming_terminal",
                                    "committed",
                                    &terminal_events,
                                );
                            }
                        }
                        Ok(TerminalTransitionOutcome::Superseded(durable)) => {
                            persist_terminal_events = false;
                            if let Some(authoritative_status) =
                                RunStatus::from_durable_status(&durable.status)
                            {
                                persisted_status = authoritative_status;
                                if let Some(run) = runs.write().await.get_mut(&bg_run_id) {
                                    run.status = authoritative_status;
                                    run.waiting_for = durable.waiting_for.clone();
                                    run.live_tx = None;
                                }
                            } else {
                                runs.write().await.remove(&bg_run_id);
                            }
                            record_durable_run_event_batch_metrics(
                                bg_metrics_registry.as_ref(),
                                "non_streaming_terminal",
                                "stale",
                                &terminal_events,
                            );
                            tracing::warn!(
                                target: "astra_runtime::run_lifecycle",
                                run_id = %bg_run_id,
                                status = persisted_status.as_str(),
                                "skipping terminal transition after stale terminal status CAS"
                            );
                        }
                        Err(error) => {
                            persist_terminal_events = false;
                            // The durable result is unknown. Remove the stale
                            // process-local projection so later reads must
                            // reconcile from the durable authority.
                            runs.write().await.remove(&bg_run_id);
                            record_durable_run_event_batch_metrics(
                                bg_metrics_registry.as_ref(),
                                "non_streaming_terminal",
                                "error",
                                &terminal_events,
                            );
                            tracing::warn!(
                                target: "astra_runtime::run_lifecycle",
                                run_id = %bg_run_id,
                                error = %error,
                                "failed to persist terminal status/events transition"
                            );
                        }
                    }
                }

                // Evict only after the durable outcome is authoritative. A
                // lost or unknown CAS must never evict based on stale local
                // completion.
                if (durable_status_committed || !persist_status_update)
                    && !persisted_status.is_resumable()
                {
                    Self::schedule_run_eviction(&runs, bg_run_id.clone());
                }
                astra_core::log_persist!(
                    run_engine
                        .persist_usage(
                            &bg_user_id,
                            &bg_run_id,
                            loop_state.provider_input_tokens(),
                            loop_state.total_completion,
                            loop_state.total_tool_calls,
                        )
                        .await,
                    "run_lifecycle",
                    &bg_run_id,
                    "usage"
                );
                // Record tokens consumed so check_token_budget sees up-to-date usage.
                if let Some(ref gov) = bg_resource_governor {
                    let total = loop_state.total_prompt + loop_state.total_completion;
                    if total > 0 {
                        gov.record_tokens(&bg_user_id, total).await;
                    }
                }
                if durable_status_committed
                    && persist_terminal_events
                    && !terminal_events.is_empty()
                    && !terminal_events_committed
                {
                    match run_engine
                        .append_events_batch(&bg_user_id, &bg_run_id, &terminal_events)
                        .await
                    {
                        Ok(()) => {
                            record_durable_run_event_batch_metrics(
                                bg_metrics_registry.as_ref(),
                                "non_streaming_terminal",
                                "committed",
                                &terminal_events,
                            );
                        }
                        Err(error) => {
                            record_durable_run_event_batch_metrics(
                                bg_metrics_registry.as_ref(),
                                "non_streaming_terminal",
                                "error",
                                &terminal_events,
                            );
                            astra_core::agent_warn!(
                                "run_lifecycle",
                                "persist append_terminal_events_batch for run {}: {}",
                                bg_run_id,
                                error
                            );
                        }
                    }
                }

                // The execution is no longer resumable once its durable final
                // state has been reconciled. Release cross-pod ownership before
                // slower observability, memory, and workspace cleanup so those
                // side effects do not extend the executor's apparent lifetime.
                drop(_owner_lease_heartbeat);

                if persist_terminal_events {
                    flush_turn_observability(&mut loop_state, &bg_session_id, false);
                    persist_turn_evaluation_journal(&bg_session_id, "server_runtime", &loop_state);
                }

                // Best-effort post-loop persistence (core events, tool events,
                // hook DB, observer, session-end hooks, promotion events).
                if let Err(e) = persist_ctx.run(&loop_state, loop_success).await {
                    tracing::error!(
                        session_id = %bg_session_id,
                        run_id = %bg_run_id,
                        error = %e,
                        "post-loop persistence failed"
                    );
                }
                if let Err(e) = persist_ctx
                    .materialize_run_transcript_evidence(&loop_state)
                    .await
                {
                    tracing::warn!(
                        session_id = %bg_session_id,
                        run_id = %bg_run_id,
                        error = %e,
                        "durable transcript evidence materialization failed"
                    );
                }

                // Post-loop memory cleanup — shared with the streaming path
                // (see `stream_chat`). By default this only schedules external
                // Memoria work so the run permit can be released promptly.
                post_loop_memory_cleanup(
                    loop_state.current_session_id.as_deref().unwrap_or(""),
                    &loop_state.session_facts,
                    loop_state.memory_extraction_service.as_ref(),
                    build_shutdown_extraction_request(&loop_state),
                    bg_metrics_registry.clone(),
                )
                .await;

                if let Some(record) = bg_cloud_workspace_record.as_ref() {
                    Self::cleanup_cloud_workspace_after_terminal_run(
                        bg_workspace_record_store,
                        &bg_user_id,
                        &bg_session_id,
                        &bg_run_id,
                        record,
                        &persisted_status,
                    )
                    .await;
                }
            },
            "agentic_loop_create_run",
        );

        Ok(ChatRunRecord {
            session_id,
            run_id,
            status: STATUS_RUNNING.to_string(),
            explain: if request.explain {
                Some(json!({"mode": "background"}))
            } else {
                None
            },
        })
    }

    /// Stream chat (incremental SSE mode): spawns the agentic loop in a
    /// background task and returns an event channel for incremental streaming.
    /// Post-loop cleanup (persistence, learning state) runs inside the task.
    async fn stream_chat(
        &self,
        user_id: String,
        request: ChatRequestData,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
        let request = self.prepare_chat_request(request).await?;
        let request_constraints = self
            .validate_request_constraints(&user_id, &request)
            .await?;
        let mut request = request;
        Self::install_agent_binding_runtime_forward_headers(&mut request)?;

        // ── Resource governance check ────────────────────────────────
        if let Some(ref gov) = self.resource_governor {
            if let astra_services::resource_governor::LimitCheck::Denied { limit, reason } =
                gov.check_run_start(&user_id).await
            {
                return Err(per_user_run_quota_response(limit, reason));
            }
        }

        let run_id = Uuid::new_v4().to_string();
        let session_id = request
            .session_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let agent_binding_mode = request.agent_binding.is_some();
        let edge_context = Self::extract_edge_context(&request)?;
        let edge_tools = edge_context.edge_tools.clone();
        let server_service_tool_catalog_enabled =
            Self::server_service_tool_catalog_enabled_for_request(
                agent_binding_mode,
                edge_context.has_tools(),
            );
        let runtime_capabilities = self
            .prepare_runtime_capabilities(&request, &request_constraints)
            .await?;
        let requires_runtime_mcp_executor = runtime_capabilities.mcp_bundle.is_some();
        let mut edge_profile = edge_context.edge_profile.to_map();
        Self::apply_agent_binding_prompt_context(
            &mut edge_profile,
            runtime_capabilities.agent_binding.as_ref(),
            request.runtime_system_prompt.as_deref(),
            request.context.as_ref(),
        );

        // Provision explicit workspace bindings early so build_initial_state
        // and durable run_started metadata use the same execution boundary.
        let cloud_workspace_record = self
            .provision_cloud_workspace_record(&user_id, &session_id, &request, &run_id)
            .await?;
        // Orchestrator-managed architecture: executor bindings come directly
        // from the workspace record — no server-owned executor scheduling.
        let cloud_execution_bindings = cloud_workspace_record
            .as_ref()
            .map(|record| execution_bindings_from_workspace_record(record));
        let cloud_workspace = cloud_workspace_record
            .as_ref()
            .map(|record| PathBuf::from(&record.root_or_volume_ref));

        let server_workspace =
            if cloud_workspace_record.is_none() && request_uses_server_workspace(&request) {
                Some(self.provision_server_workspace(&session_id)?)
            } else {
                None
            };
        let execution_bindings = cloud_execution_bindings
            .or_else(|| {
                server_workspace.as_deref().map(|workspace| {
                    let (workspace, executor) =
                        resolve_request_execution_bindings(&request, workspace);
                    ExecutionBindingSnapshot::inferred(workspace, executor)
                })
            })
            .or_else(|| {
                resolve_request_execution_bindings_without_server_workspace(&request, &edge_profile)
                    .map(|(workspace, executor)| {
                        ExecutionBindingSnapshot::inferred(workspace, executor)
                    })
            });
        if let Err(error) = self
            .validate_optional_tool_availability(
                &user_id,
                &request_constraints,
                execution_bindings.as_ref(),
            )
            .await
        {
            if let Some(record) = cloud_workspace_record.as_ref() {
                self.cleanup_cloud_workspace_after_failed_start(
                    &user_id,
                    &session_id,
                    &run_id,
                    record,
                    "enabled optional tools became unavailable before stream start".to_string(),
                )
                .await;
            }
            return Err(error);
        }
        let tool_runtime_workspace = cloud_workspace.clone().or_else(|| server_workspace.clone());
        let server_tool_executor_workspace = if let Some(workspace) = tool_runtime_workspace.clone()
        {
            Some(workspace)
        } else if !agent_binding_mode || requires_runtime_mcp_executor {
            Some(self.provision_server_workspace(&session_id)?)
        } else {
            None
        };
        let stream_agent_spawner = self
            .server_agent_spawner_for_session(&user_id, &session_id)
            .await
            .spawner;

        // Network observer delivery is bounded. Internal producers are
        // drained independently below so browser backpressure cannot drop an
        // approval or permanently detach later host progress.
        const SSE_CHANNEL_CAPACITY: usize = 512;
        let (client_event_tx, event_rx) = mpsc::channel::<Value>(SSE_CHANNEL_CAPACITY);
        let (event_tx, mut fanout_rx) = mpsc::channel::<Value>(SSE_CHANNEL_CAPACITY);
        let (agent_live_gap_tracker, mut agent_live_gap_rx) = WorkSurfaceAgentLiveGapTracker::new();
        let (live_tx, _) = broadcast::channel::<Value>(SSE_CHANNEL_CAPACITY);
        let live_tx_for_fanout = live_tx.clone();
        let mut client_event_tx_for_fanout = Some(client_event_tx.clone());
        let fanout_runs = self.runs_handle();
        let fanout_run_engine = self.run_engine.clone();
        let fanout_user_id = user_id.clone();
        let fanout_run_id = run_id.clone();
        let fanout_gap_tracker = agent_live_gap_tracker.clone();
        spawn_observed(
            async move {
                let mut gap_watch_open = true;
                loop {
                    tokio::select! {
                        event = fanout_rx.recv() => {
                            let Some(mut event) = event else {
                                for gap in fanout_gap_tracker.drain() {
                                    let event = agent_live_gap_to_work_surface_sse(gap);
                                    let _ = live_tx_for_fanout.send(event.clone());
                                    send_attached_stream_event(
                                        &mut client_event_tx_for_fanout,
                                        event,
                                        &fanout_run_id,
                                    )
                                    .await;
                                }
                                break;
                            };
                            let interaction_already_committed = event
                                .as_object_mut()
                                .and_then(|event| event.remove(HOST_INTERACTION_COMMITTED_FIELD))
                                .and_then(|value| value.as_bool())
                                .unwrap_or(false);
                            let approval_requests = canonical_edge_approval_requests(&event);
                            let approval_run_id = event
                                .get("run_id")
                                .and_then(Value::as_str)
                                .map(str::trim)
                                .filter(|run_id| !run_id.is_empty())
                                .unwrap_or(&fanout_run_id)
                                .to_string();
                            if !interaction_already_committed
                                && !approval_requests.is_empty()
                                && let Err(error) = fanout_run_engine
                                    .append_events_batch(
                                        &fanout_user_id,
                                        &approval_run_id,
                                        &approval_requests,
                                    )
                                    .await
                            {
                                // Never expose an approval button whose identity
                                // cannot be authenticated by the callback path.
                                // Cancel the live executor and surface the
                                // persistence failure instead of waiting for a
                                // response that can only be rejected.
                                if let Some(run) = fanout_runs.write().await.get_mut(&fanout_run_id) {
                                    run.cancel_flag.store(true, Ordering::SeqCst);
                                    run.llm_cancel_token.cancel();
                                }
                                let failure = json!({
                                    "type": "run_error",
                                    "error": "approval request could not be recorded durably",
                                    "error_code": "approval_persistence_failed",
                                });
                                tracing::error!(
                                    target: "astra_runtime::run_lifecycle",
                                    user_id = %fanout_user_id,
                                    run_id = %approval_run_id,
                                    error = %error,
                                    "edge approval request persistence failed before delivery"
                                );
                                let _ = live_tx_for_fanout.send(failure.clone());
                                send_attached_stream_event(
                                    &mut client_event_tx_for_fanout,
                                    failure,
                                    &fanout_run_id,
                                )
                                .await;
                                break;
                            }
                            if live_delta_event_for_persistence(&event) {
                                if let Some(run) = fanout_runs.write().await.get_mut(&fanout_run_id) {
                                    push_active_run_live_event(run, event.clone());
                                }
                            }
                            let _ = live_tx_for_fanout.send(event.clone());
                            send_attached_stream_event(
                                &mut client_event_tx_for_fanout,
                                event,
                                &fanout_run_id,
                            )
                            .await;
                        }
                        changed = agent_live_gap_rx.changed(), if gap_watch_open => {
                            if changed.is_err() {
                                gap_watch_open = false;
                                continue;
                            }
                            for gap in fanout_gap_tracker.drain() {
                                let event = agent_live_gap_to_work_surface_sse(gap);
                                let _ = live_tx_for_fanout.send(event.clone());
                                send_attached_stream_event(
                                    &mut client_event_tx_for_fanout,
                                    event,
                                    &fanout_run_id,
                                )
                                .await;
                            }
                        }
                    }
                }
            },
            "sse_fanout",
        );
        let progress_bridge =
            self.spawn_agent_progress_stream_bridge(run_id.clone(), event_tx.clone());

        let (mut run_state, cancel_flag, pause_flag, llm_cancel_token) =
            Self::build_tracked_run_state(run_id.clone(), session_id.clone(), user_id.clone());
        run_state.live_tx = Some(live_tx.clone());

        let interaction_sink: Arc<dyn server_loop_host::HostInteractionSink> =
            Arc::new(DurableHostInteractionSink {
                run_engine: self.run_engine.clone(),
                user_id: user_id.clone(),
                run_id: run_id.clone(),
                session_id: session_id.clone(),
                agent_id: None,
                event_tx: Some(event_tx.clone()),
            });
        let mut state = self.build_initial_state_inner(
            &user_id,
            &request,
            &session_id,
            &run_id,
            tool_runtime_workspace
                .as_deref()
                .or(server_workspace.as_deref()),
            execution_bindings.as_ref(),
            Some(llm_cancel_token.clone()),
            Some(Arc::clone(&interaction_sink)),
            request_constraints.clone(),
            &edge_context,
            Some(&edge_profile),
            runtime_capabilities.request_scoped_skill_resolver.clone(),
            runtime_capabilities.agent_binding.as_ref(),
        );
        state.context_manifest_user_id = Some(user_id.clone());
        state.runtime_manifest = Self::build_runtime_manifest(
            &request,
            &runtime_capabilities,
            tool_runtime_workspace.is_some(),
        );
        // Inject user_id into the harness sink used by DB-persistence tests.
        #[cfg(feature = "harness")]
        state.harness.set_user_id(&user_id);

        state.session_turn =
            infer_session_turn(self.shared_pool.as_ref(), &user_id, &session_id).await;
        let fresh_session_current_date = state
            .pipeline_session
            .as_ref()
            .map(|session| session.current_date().to_string())
            .unwrap_or_else(|| {
                crate::turn::session_current_date::resolve_session_current_date(&session_id)
            });

        // ── Runtime warm-start from step checkpoint ────────────────
        let restore_prior_prompt_history = should_restore_prior_prompt_history(
            request.session_id.is_some(),
            self.session_has_prior_prompt_history(&user_id, &session_id)
                .await,
        );

        if restore_prior_prompt_history {
            if let Ok(Some(restored)) =
                astra_pipeline::step_restore::restore_session(&user_id, &session_id)
            {
                restore_step_checkpoint_runtime_state(
                    restored,
                    &fresh_session_current_date,
                    &mut state,
                );
            }
        }

        // ── CSL: Load conversation history from the log ─────────────
        let csl_manager = if restore_prior_prompt_history {
            self.restore_csl_history(&user_id, &session_id, &run_id, &mut state)
                .await
        } else {
            None
        };

        let plan_resume_snapshot = if let Some(shared) = &self.shared_pool {
            let repo = astra_plan::CloudPlanRepository::new(shared.get().clone());
            astra_plan::plan_resume_snapshot_for_session(&repo, &user_id, &session_id).await
        } else {
            astra_plan::PlanResumeSnapshot::default()
        };
        let session_resume_hint = self
            .session_resume_hydration_hint_for_session(
                &user_id,
                &session_id,
                &run_id,
                restore_prior_prompt_history,
            )
            .await;
        let plan_resume_hint = astra_turn_core::resume_hydration::merge_resume_hints(
            session_resume_hint,
            plan_resume_snapshot.prompt_hint,
        );
        let plan_authoring_active = plan_resume_snapshot.authoring_active;
        let task_board_resume_hint = self
            .task_board_resume_hint_for_session(&user_id, &session_id)
            .await;
        let mut host = self.build_host(
            &user_id,
            &session_id,
            &run_id,
            &request,
            edge_tools,
            edge_profile.clone(),
            server_service_tool_catalog_enabled,
            !agent_binding_mode,
            execution_bindings.as_ref(),
            plan_resume_hint,
            plan_authoring_active,
            task_board_resume_hint,
        );
        self.configure_host_approval_audit_context(
            &mut host,
            &user_id,
            &session_id,
            &run_id,
            state.session_turn,
            request.agent_id.clone(),
        );
        const HOST_EVENT_CHANNEL_CAPACITY: usize = 256;
        let (host_event_tx, mut host_event_rx) =
            mpsc::channel::<Value>(HOST_EVENT_CHANNEL_CAPACITY);
        let host_event_gap = server_loop_host::HostEventGapTracker::default();
        let bridge_gap = host_event_gap.clone();
        let host_event_bridge_tx = event_tx.clone();
        let host_event_bridge_run_id = run_id.clone();
        let mut host_event_bridge = tokio::spawn(async move {
            loop {
                tokio::select! {
                    event = host_event_rx.recv() => {
                        let Some(event) = event else { break; };
                        let dropped = bridge_gap.take();
                        if dropped > 0
                            && host_event_bridge_tx
                                .send(stream_delivery_gap_event(
                                    &host_event_bridge_run_id,
                                    dropped,
                                ))
                                .await
                                .is_err()
                        {
                            return;
                        }
                        if host_event_bridge_tx.send(event).await.is_err() {
                            return;
                        }
                    }
                    _ = bridge_gap.notified() => {
                        let dropped = bridge_gap.take();
                        if dropped > 0
                            && host_event_bridge_tx
                                .send(stream_delivery_gap_event(
                                    &host_event_bridge_run_id,
                                    dropped,
                                ))
                                .await
                                .is_err()
                        {
                            return;
                        }
                    }
                }
            }
            let dropped = bridge_gap.take();
            if dropped > 0 {
                let _ = host_event_bridge_tx
                    .send(stream_delivery_gap_event(
                        &host_event_bridge_run_id,
                        dropped,
                    ))
                    .await;
            }
        });
        host.set_event_tx_with_gap(host_event_tx, host_event_gap);
        host.set_interaction_sink(interaction_sink);
        host.set_client_cancel(cancel_flag.clone(), llm_cancel_token.clone());
        if let Some(snapshot) = execution_bindings.as_ref() {
            host.set_execution_metadata(Value::Object(binding_event_fields(
                &snapshot.workspace,
                &snapshot.executor,
            )));
        }

        // ── MCP: inject request-scoped schemas into host tool surface ─
        if let Some(ref bundle) = runtime_capabilities.mcp_bundle {
            host.install_runtime_tool_schemas(bundle.schemas.clone(), bundle.control_tools.clone());
            host.install_runtime_stop_after_success_tools(bundle.stop_after_success_tools.clone());
        }
        // In agent-binding mode with an EdgeAgent executor, the host only installs
        // MCP schemas by default. Merge edge-builtin tool schemas (bash, read_file, …)
        // for any tools explicitly listed in allow_tools so the model can see and call
        // them via the edge dispatch path.
        if let Some(snapshot) = execution_bindings.as_ref() {
            if snapshot.executor.kind == ExecutorBindingKind::EdgeAgent {
                if let Some(allow_tools) = request.allow_tools.as_deref() {
                    let tools: Vec<String> = allow_tools.iter().map(|s| s.to_string()).collect();
                    host.merge_allowlisted_edge_tool_schemas(&tools);
                }
            }
        }

        // Guard: reject if this session already has a blocking run.
        // Hold write lock across check+insert to prevent TOCTOU race.
        {
            let mut runs = self.runs.write().await;
            let has_active = Self::session_has_blocking_run(&runs, &session_id);
            if has_active {
                if let Some(record) = cloud_workspace_record.as_ref() {
                    self.cleanup_cloud_workspace_after_failed_start(
                        &user_id,
                        &session_id,
                        &run_id,
                        record,
                        "session already has an active run before streaming agentic loop start"
                            .to_string(),
                    )
                    .await;
                }
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "session already has an active run".to_string(),
                ));
            }
            runs.insert(run_id.clone(), run_state);
        }
        // Persist run first, so the binding is durable before the client
        // receives binding events and starts using the workspace.
        if let Err(error) = self
            .persist_run_start(
                &run_id,
                &user_id,
                &session_id,
                &request,
                execution_bindings.as_ref(),
                runtime_capabilities.agent_binding.as_ref(),
            )
            .await
        {
            self.runs.write().await.remove(&run_id);
            if let Some(record) = cloud_workspace_record.as_ref() {
                self.cleanup_cloud_workspace_after_failed_start(
                    &user_id,
                    &session_id,
                    &run_id,
                    record,
                    format!(
                        "durable streaming run start failed after cloud workspace provisioning: {}",
                        error.1.0.detail
                    ),
                )
                .await;
            }
            return Err(error);
        }
        if let Some(snapshot) = execution_bindings.as_ref() {
            for event in binding_snapshot_events(
                &run_id,
                &session_id,
                &snapshot.workspace,
                &snapshot.executor,
            ) {
                if event_tx.send(event).await.is_err() {
                    self.fail_started_run_before_spawn(
                        &user_id,
                        &run_id,
                        "failed to start streaming run event stream",
                        PreSpawnFailureCode::PreSpawnFailure,
                    )
                    .await;
                    if let Some(record) = cloud_workspace_record.as_ref() {
                        self.cleanup_cloud_workspace_after_failed_start(
                            &user_id,
                            &session_id,
                            &run_id,
                            record,
                            "failed to start streaming run event stream after cloud workspace provisioning"
                                .to_string(),
                        )
                        .await;
                    }
                    return Err(error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "failed to start run event stream".to_string(),
                    ));
                }
            }
        }
        if let Some(pool) = &self.shared_pool {
            let trace = server_trace_context(&user_id, &session_id, &run_id, state.session_turn);
            let user_transcript = TranscriptPersistItem {
                run_id: Some(run_id.clone()),
                role: "user",
                content: request.message.clone(),
                payload: None,
                source_event_id: trace.root_event_id,
            };
            if let Err(error) =
                persist_session_transcript_items(pool, &user_id, &session_id, &[user_transcript])
                    .await
            {
                tracing::warn!(
                    %session_id,
                    %error,
                    "user intent transcript item was not committed"
                );
            }
        }

        self.configure_loop_state_runtime_controls(
            &mut state,
            &cancel_flag,
            &pause_flag,
            (*llm_cancel_token).clone(),
        );
        configure_runtime_controllers(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &mut state,
            &user_id,
            &session_id,
        )
        .await;

        // Wire the server-side runtime tool owner whenever the host exposes the
        // server tool catalog. For edge-bound runs this uses an internal
        // scratch workspace only; the visible binding still routes local-code
        // tools to edge or blocks when edge is unavailable.
        if let Some(workspace) = server_tool_executor_workspace {
            let memoria_base = Some(astra_core::MemoriaSettings::from_env().base_url);
            let task_store = match astra_tools::task_mgmt_matrixone::select_task_store(
                self.shared_pool.as_ref().map(|p| p.get().clone()),
                user_id.clone(),
            ) {
                Ok(store) => store,
                Err(error) => {
                    let message = format!(
                        "streaming tool executor setup failed after durable run start: {error}"
                    );
                    self.fail_started_run_before_spawn(
                        &user_id,
                        &run_id,
                        &message,
                        PreSpawnFailureCode::PreSpawnFailure,
                    )
                    .await;
                    if let Some(record) = cloud_workspace_record.as_ref() {
                        self.cleanup_cloud_workspace_after_failed_start(
                            &user_id,
                            &session_id,
                            &run_id,
                            record,
                            message,
                        )
                        .await;
                    }
                    return Err(error_response(StatusCode::INTERNAL_SERVER_ERROR, error));
                }
            };
            let mut executor = runtime_tool_executor::RuntimeToolExecutor::new(
                workspace.clone(),
                user_id.clone(),
                session_id.clone(),
                memoria_base,
                None,
            );
            executor = wire_reflect_service_into_executor(executor, &self.reflect_service)
                .with_cancel_token(state.cancellation.token.clone())
                .with_task_store(task_store);
            if agent_binding_mode {
                executor = executor.with_server_service_tools_disabled();
            } else {
                executor =
                    executor.with_capabilities(crate::capabilities::lifecycle_server_capabilities(
                        self.shared_pool.is_some(),
                        self.reflect_service.is_configured(),
                    ));
            }

            // Apply shared ToolExecutionService (with admin-controllable disabled_tool_offers)
            // or fall back to building one from deployment config.
            if let Some(ref shared_tes) = self.tool_execution_service {
                executor = executor.with_tool_execution_service(shared_tes.clone());
            } else {
                let mut builder = apply_deployment_tool_policy(
                    ToolExecutionService::builder(),
                    &load_deployment_tool_policy(),
                );
                if let Some(pool) = &self.edge_connection_pool {
                    builder = builder.edge_connection_pool(pool.clone());
                }
                if let Some(svc) = &self.edge_dispatch_service {
                    builder = builder.edge_dispatch_service(Arc::clone(svc));
                }
                if let Some(svc) = &self.edge_registry_service {
                    builder = builder.edge_registry_service(Arc::clone(svc));
                }
                executor = executor.with_tool_execution_service(builder.build());
            }

            // ── MCP: inject request-scoped provider state into executor ────
            if let Some(ref bundle) = runtime_capabilities.mcp_bundle {
                if let Some(manager) = &bundle.manager {
                    executor.set_mcp_manager(manager.clone());
                }
                if let Some(agent_binding_mcp) = &bundle.agent_binding_mcp {
                    executor.set_agent_binding_mcp(agent_binding_mcp.clone());
                }
                executor.set_request_scoped_mcp_schemas(bundle.schemas.clone());
                executor.set_provider_policy_index(bundle.provider_policy_index.clone());
            }
            if let Some(shared) = &self.shared_pool {
                executor.set_context_manifest_pool(shared.clone());
                executor = executor.with_session_artifact_store(
                    astra_services::DatabaseSessionArtifactStore::new(self.matrixone.clone())
                        .with_pool(shared.clone()),
                );
                executor.set_plan_repository(std::sync::Arc::new(
                    astra_plan::CloudPlanRepository::new(shared.get().clone()),
                ));
            }
            executor.enable_durable_invocations();
            configure_runtime_semantic_read_cache(
                &mut executor,
                runtime_capabilities.mcp_bundle.as_ref(),
            );
            if let Some(observability_session) = state.telemetry.observability_session.clone() {
                executor.set_observability_session(observability_session);
            }
            if let Some(writer) = self.auxiliary_event_writer.clone() {
                executor.set_auxiliary_event_writer(writer);
            }
            let binding_snapshot = execution_bindings.clone().unwrap_or_else(|| {
                let (workspace_binding, executor_binding) =
                    resolve_request_execution_bindings(&request, workspace.as_path());
                ExecutionBindingSnapshot::inferred(workspace_binding, executor_binding)
            });
            let agent_working_dir =
                agent_working_dir_for_bindings(execution_bindings.as_ref(), workspace.as_path());
            executor.set_execution_binding_snapshot(binding_snapshot);
            executor.set_workspace_record(provider_effective_workspace_record(
                cloud_workspace_record.as_ref(),
                request.provider_workspace_id.as_deref(),
            ));
            host.set_execution_metadata(executor.binding_metadata());
            executor.set_work_surface_event_tx(event_tx.clone());
            let active_work_registry = self
                .wire_server_dynamic_agent_tools(
                    &mut executor,
                    &user_id,
                    &session_id,
                    &run_id,
                    state.session_turn,
                    &request,
                    agent_working_dir.as_path(),
                    Some(event_tx.clone()),
                    Some(agent_live_gap_tracker.clone()),
                    Some(pause_flag.clone()),
                    Some(llm_cancel_token.clone()),
                    #[cfg(feature = "harness")]
                    state.harness.sink.clone(),
                )
                .await;
            state.attach_active_work_registry(active_work_registry);

            // Match the background-run interaction contract.  The stream is
            // only a delivery surface: Server Only requests still wait on the
            // same shared durable interaction and can be resolved
            // after this particular SSE connection disappears.
            if request.interactive_client {
                let (approval_tx, approval_rx) = mpsc::channel::<Value>(64);
                let approval_gate = DurableRunApprovalGate::new(
                    user_id.clone(),
                    session_id.clone(),
                    run_id.clone(),
                    Some(state.session_turn),
                    self.run_engine.clone(),
                    self.runs_handle(),
                    Some(approval_tx),
                    Some(event_tx.clone()),
                )
                .with_cancel_token(llm_cancel_token.clone());
                executor.set_approval_gate(std::sync::Arc::new(approval_gate));
                self.approval_channels
                    .lock()
                    .await
                    .insert(run_id.clone(), approval_rx);

                let (user_prompt_tx, user_prompt_rx) = mpsc::channel::<Value>(64);
                let user_prompt_gate = DurableRunUserPromptGate::new(
                    user_id.clone(),
                    session_id.clone(),
                    run_id.clone(),
                    Some(state.session_turn),
                    self.run_engine.clone(),
                    self.runs_handle(),
                    Some(user_prompt_tx),
                    Some(event_tx.clone()),
                )
                .with_cancel_token(llm_cancel_token.clone());
                executor.set_ask_user_gate(std::sync::Arc::new(user_prompt_gate));
                self.user_prompt_channels
                    .lock()
                    .await
                    .insert(run_id.clone(), user_prompt_rx);

                let (progress_tx, progress_rx) = mpsc::channel::<ProgressEvent>(64);
                let progress_cb =
                    astra_server_types::ws_progress_callback::WebSocketProgressCallback::new(
                        progress_tx,
                    );
                executor.set_progress_callback(std::sync::Arc::new(progress_cb));
                self.progress_channels
                    .lock()
                    .await
                    .insert(run_id.clone(), progress_rx);
            } else {
                executor.set_approval_gate(std::sync::Arc::new(
                    DurableRunApprovalGate::new(
                        user_id.clone(),
                        session_id.clone(),
                        run_id.clone(),
                        Some(state.session_turn),
                        self.run_engine.clone(),
                        self.runs_handle(),
                        None,
                        Some(event_tx.clone()),
                    )
                    .with_cancel_token(llm_cancel_token.clone()),
                ));
                executor.set_ask_user_gate(std::sync::Arc::new(
                    DurableRunUserPromptGate::new(
                        user_id.clone(),
                        session_id.clone(),
                        run_id.clone(),
                        Some(state.session_turn),
                        self.run_engine.clone(),
                        self.runs_handle(),
                        None,
                        Some(event_tx.clone()),
                    )
                    .with_cancel_token(llm_cancel_token.clone()),
                ));
            }
            wire_executor_into_state(executor, &mut state);
        }

        // Clone handles for the background task.
        let runs = self.runs_handle();
        let run_engine = self.run_engine.clone();
        let bg_run_id = run_id.clone();
        let bg_session_id = session_id.clone();
        let bg_resource_governor = self.resource_governor.clone();
        let bg_user_id = user_id.clone();
        let bg_cloud_workspace_record = cloud_workspace_record.clone();
        let bg_workspace_record_store = self.workspace_record_store.clone();
        let missing_lifecycle_spawner = Arc::clone(&stream_agent_spawner);
        let bg_metrics_registry = self.metrics_registry.clone();
        let bg_cancel_flag = cancel_flag.clone();
        let bg_pause_flag = pause_flag.clone();
        let bg_llm_cancel_token = llm_cancel_token.clone();
        let bg_root_mailbox_router = Arc::clone(&self.server_agent_mailbox_router);
        let bg_root_mailbox_agent_id = request
            .agent_id
            .clone()
            .unwrap_or_else(|| "root-agent".to_string());
        let persist_ctx = PostLoopPersistContext {
            matrixone: self.matrixone.clone(),
            shared_pool: self.shared_pool.clone(),
            user_id: user_id.clone(),
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            agent_id: request.agent_id.clone(),
            model_name: request.model.clone(),
            user_message: request.message.clone(),
            hook_db_writer: self.hook_db_writer.clone(),
            observer_worker: self.observer_worker.clone(),
            tool_event_writer: self.tool_event_writer.clone(),
            metrics_registry: self.metrics_registry.clone(),
            csl_manager: csl_manager.map(tokio::sync::Mutex::new),
        };

        // ── Global admission control: limit concurrent agentic loop tasks ──
        // Wait for a configured interval before returning a structured 503.
        let permit = match self.acquire_run_permit(run_admission_timeout()).await {
            Ok(permit) => permit,
            Err(error) => {
                let failure_reason = match error {
                    RunAdmissionError::Timeout => {
                        "server capacity timeout before streaming agentic loop start"
                    }
                    RunAdmissionError::Closed => {
                        "server capacity admission closed before streaming agentic loop start"
                    }
                };
                self.fail_started_run_before_spawn(&user_id, &run_id, failure_reason, error.into())
                    .await;
                self.remove_run_channels(&run_id).await;
                if let Some(record) = cloud_workspace_record.as_ref() {
                    self.cleanup_cloud_workspace_after_failed_start(
                        &user_id,
                        &session_id,
                        &run_id,
                        record,
                        failure_reason.to_string(),
                    )
                    .await;
                }
                return Err(run_admission_capacity_response(error));
            }
        };

        // Background task tracking (same pattern as the create_run spawn above).
        // Spawn the agentic loop in a background task. Events are pushed
        // through event_tx incrementally; the HTTP handler streams them.
        let bg_task_count_2 = Arc::clone(&self.background_task_count);
        bg_task_count_2.fetch_add(1, Ordering::Release);
        spawn_observed(
            async move {
                let _permit = permit; // RAII: released when this task completes
                struct TaskCountGuard(Arc<AtomicUsize>);
                impl Drop for TaskCountGuard {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::Release);
                    }
                }
                let _guard = TaskCountGuard(bg_task_count_2);
                let _owner_lease_heartbeat =
                    run_engine.start_owner_lease_heartbeat(bg_user_id.clone(), bg_run_id.clone());
                if let Some(ref gov) = bg_resource_governor {
                    use astra_services::resource_governor::LimitCheck;
                    if let LimitCheck::Denied { limit, reason } =
                        gov.check_token_budget(&bg_user_id).await
                    {
                        tracing::warn!(
                            target: "astra_runtime::run_lifecycle",
                            user_id = %bg_user_id,
                            run_id = %bg_run_id,
                            limit = %limit.as_str(),
                            reason = %reason,
                            "streaming run rejected: daily token budget exhausted"
                        );
                        let terminal_events = Self::persist_started_run_quota_rejection(
                            &run_engine,
                            &runs,
                            &bg_user_id,
                            &bg_run_id,
                            limit,
                            &reason,
                        )
                        .await;
                        if let Some(terminal_events) = terminal_events {
                            for event in run_handlers::transform_stream_run_events_for_client(
                                &bg_run_id,
                                terminal_events,
                            ) {
                                if event_tx.send(event).await.is_err() {
                                    break;
                                }
                            }
                            Self::schedule_run_eviction(&runs, bg_run_id.clone());
                            if let Some(record) = bg_cloud_workspace_record.as_ref() {
                                Self::cleanup_cloud_workspace_after_terminal_run(
                                    bg_workspace_record_store.clone(),
                                    &bg_user_id,
                                    &bg_session_id,
                                    &bg_run_id,
                                    record,
                                    &RunStatus::Failed,
                                )
                                .await;
                            }
                        }
                        drop(event_tx);
                        return;
                    }
                }
                install_server_root_mailbox(
                    &mut state,
                    &bg_root_mailbox_router,
                    &bg_session_id,
                    &bg_run_id,
                    &bg_root_mailbox_agent_id,
                )
                .await;
                let _control_watcher = start_active_run_control_watcher(
                    state.run_control.clone(),
                    bg_user_id.clone(),
                    bg_run_id.clone(),
                    bg_cancel_flag.clone(),
                    bg_pause_flag.clone(),
                    bg_llm_cancel_token.clone(),
                );
                let loop_result =
                    run_agentic_loop_with_host_panic_safe(&mut host, &mut state).await;
                host.detach_event_tx();
                match tokio::time::timeout(Duration::from_secs(2), &mut host_event_bridge).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::warn!(
                        target: "astra_runtime::run_lifecycle",
                        run_id = %bg_run_id,
                        error = %error,
                        "bounded host event bridge stopped before draining"
                    ),
                    Err(_) => {
                        host_event_bridge.abort();
                        let _ = host_event_bridge.await;
                        tracing::warn!(
                            target: "astra_runtime::run_lifecycle",
                            run_id = %bg_run_id,
                            "bounded host event bridge drain exceeded 2 seconds; detached remaining live progress"
                        );
                    }
                }
                park_server_root_mailbox(&mut state).await;
                let (loop_result, emitted_events) = host.settle_loop_turn(loop_result);
                let loop_success = loop_result.is_ok();

                // Best-effort post-loop persistence (core events, tool events,
                // hook DB, observer, session-end hooks, promotion events).
                if let Err(e) = persist_ctx.run(&state, loop_success).await {
                    tracing::error!(
                        session_id = %bg_session_id,
                        run_id = %bg_run_id,
                        error = %e,
                        "post-loop persistence failed"
                    );
                }

                let (final_events, final_status, error_msg) =
                    Self::finalize_run_events(loop_result, emitted_events, &state);
                if matches!(&final_status, RunStatus::Cancelled) {
                    let cancelled_children = missing_lifecycle_spawner
                        .cancel_descendants_of_parent_run(
                            &bg_run_id,
                            DescendantCancellationReason::AncestorCancelled,
                        )
                        .await;
                    if cancelled_children > 0 {
                        tracing::info!(
                            target: "astra_runtime::run_lifecycle",
                            run_id = %bg_run_id,
                            cancelled_children,
                            "cancelled dynamic-agent descendants with parent run"
                        );
                    }
                    if let Err(error) = Self::cancel_durable_run_descendants(
                        &run_engine,
                        &bg_user_id,
                        &bg_session_id,
                        &bg_run_id,
                        DescendantCancellationReason::AncestorCancelled.as_str(),
                    )
                    .await
                    {
                        tracing::warn!(
                            target: "astra_runtime::run_lifecycle",
                            run_id = %bg_run_id,
                            error = %error,
                            "durable descendant cancellation sweep failed"
                        );
                    }
                }
                // Ensure fast synchronous child-agent progress has reached both
                // durable replay and the live SSE stream before parent terminal
                // markers close the turn.
                let sent_lifecycle_events = progress_bridge.stop_and_drain().await;
                let missing_lifecycle_events = collect_missing_agent_lifecycle_events(
                    missing_lifecycle_spawner.as_ref(),
                    &bg_run_id,
                    &sent_lifecycle_events,
                )
                .await;
                let archived_lifecycle_events = collect_agent_lifecycle_events_for_persistence(
                    missing_lifecycle_spawner.as_ref(),
                    &bg_run_id,
                )
                .await;
                // In streaming mode, host-emitted `type` events have already gone
                // through event_tx and the fanout persistence path. Replay only the
                // synthesized terminal events appended by finalize_run_events.
                let streaming_final_events: Vec<Value> = final_events
                    .iter()
                    .filter(|event| streaming_final_event_for_replay(event))
                    .cloned()
                    .collect();
                let streamed_final_events = run_handlers::transform_stream_run_events_for_client(
                    &bg_run_id,
                    streaming_final_events.clone(),
                );
                let terminal_persistence_events = merge_agent_lifecycle_before_terminal_events(
                    &final_events,
                    &archived_lifecycle_events,
                )
                .into_iter()
                .filter(|event| !incrementally_persisted_edge_interaction_event(event))
                .collect();
                let streaming_events_for_durable =
                    enforce_durable_run_event_batch_budget(terminal_persistence_events);
                record_durable_run_event_batch_metrics(
                    bg_metrics_registry.as_ref(),
                    "streaming_terminal",
                    "planned",
                    &streaming_events_for_durable,
                );
                persist_turn_evaluation_journal(&bg_session_id, "server_runtime", &state);
                let mut terminal_state_events = streaming_final_events;

                let mut persisted_status = final_status;
                let mut persist_status_update = true;
                let mut persist_streaming_events = true;
                let mut publish_stream_terminal = true;
                if let Some(run) = runs.write().await.get_mut(&bg_run_id) {
                    run.execution_live = false;
                    if run.status == RunStatus::Cancelled {
                        persist_status_update = false;
                        persist_streaming_events = false;
                        publish_stream_terminal = false;
                        merge_cancelled_run_events(run, terminal_state_events);
                        if final_status != RunStatus::Waiting {
                            run.live_tx = None;
                        }
                        flush_turn_observability(&mut state, &bg_session_id, true);
                    } else {
                        run.events.append(&mut terminal_state_events);
                        if should_preserve_manual_pause_on_completion(&run.status, &final_status) {
                            persist_status_update = false;
                            persisted_status = RunStatus::Paused;
                            run.waiting_for
                                .get_or_insert_with(|| "user_resume".to_string());
                            run.live_tx = None;
                        } else if run.status.try_transition(&final_status).is_ok() {
                            run.status = final_status;
                        }
                        if !run.status.is_resumable() {
                            run.live_tx = None;
                        }
                        flush_turn_observability(&mut state, &bg_session_id, false);
                    }
                }

                if persist_status_update
                    && should_preserve_manual_pause_from_durable(
                        &run_engine,
                        &bg_user_id,
                        &bg_run_id,
                        &final_status,
                    )
                    .await
                {
                    persist_status_update = false;
                    persisted_status = RunStatus::Paused;
                    if let Some(run) = runs.write().await.get_mut(&bg_run_id) {
                        if run.status == RunStatus::Paused
                            || run.status.try_transition(&RunStatus::Paused).is_ok()
                        {
                            run.status = RunStatus::Paused;
                            run.pause_flag.store(true, Ordering::SeqCst);
                            run.waiting_for
                                .get_or_insert_with(|| "user_resume".to_string());
                            run.live_tx = None;
                        } else {
                            tracing::warn!(
                                target: "astra_runtime::run_lifecycle",
                                run_id = %bg_run_id,
                                current_status = %run.status.as_str(),
                                "durable pause projection arrived after an incompatible local terminal state"
                            );
                        }
                    }
                }

                // Record tokens consumed regardless of cancel — cancelled runs still
                // consumed tokens and must count toward the daily budget.
                if let Some(ref gov) = bg_resource_governor {
                    let total = state.total_prompt + state.total_completion;
                    if total > 0 {
                        gov.record_tokens(&bg_user_id, total).await;
                    }
                }

                let mut durable_status_committed = !persist_status_update;
                let mut streaming_events_committed = false;
                if !persist_streaming_events {
                    record_durable_run_event_batch_metrics(
                        bg_metrics_registry.as_ref(),
                        "streaming_terminal",
                        "skipped",
                        &streaming_events_for_durable,
                    );
                }
                if persist_status_update {
                    let events_for_transition: &[Value] = if persist_streaming_events {
                        streaming_events_for_durable.as_slice()
                    } else {
                        &[]
                    };
                    match run_engine
                        .commit_terminal_status_with_events_if_current(
                            &bg_user_id,
                            &bg_run_id,
                            &[STATUS_RUNNING, STATUS_WAITING],
                            persisted_status.as_str(),
                            None,
                            error_msg.as_deref(),
                            events_for_transition,
                        )
                        .await
                    {
                        Ok(TerminalTransitionOutcome::Committed) => {
                            durable_status_committed = true;
                            streaming_events_committed = persist_streaming_events
                                && !streaming_events_for_durable.is_empty();
                            if streaming_events_committed {
                                record_durable_run_event_batch_metrics(
                                    bg_metrics_registry.as_ref(),
                                    "streaming_terminal",
                                    "committed",
                                    &streaming_events_for_durable,
                                );
                            }
                        }
                        Ok(TerminalTransitionOutcome::Superseded(durable)) => {
                            persist_streaming_events = false;
                            publish_stream_terminal = false;
                            if let Some(authoritative_status) =
                                RunStatus::from_durable_status(&durable.status)
                            {
                                persisted_status = authoritative_status;
                                if let Some(run) = runs.write().await.get_mut(&bg_run_id) {
                                    run.status = authoritative_status;
                                    run.waiting_for = durable.waiting_for.clone();
                                    run.live_tx = None;
                                }
                            } else {
                                runs.write().await.remove(&bg_run_id);
                            }
                            record_durable_run_event_batch_metrics(
                                bg_metrics_registry.as_ref(),
                                "streaming_terminal",
                                "stale",
                                &streaming_events_for_durable,
                            );
                            tracing::warn!(
                                target: "astra_runtime::run_lifecycle",
                                run_id = %bg_run_id,
                                status = persisted_status.as_str(),
                                "skipping streaming terminal transition after stale terminal status CAS"
                            );
                        }
                        Err(error) => {
                            persist_streaming_events = false;
                            publish_stream_terminal = false;
                            runs.write().await.remove(&bg_run_id);
                            record_durable_run_event_batch_metrics(
                                bg_metrics_registry.as_ref(),
                                "streaming_terminal",
                                "error",
                                &streaming_events_for_durable,
                            );
                            tracing::warn!(
                                target: "astra_runtime::run_lifecycle",
                                run_id = %bg_run_id,
                                error = %error,
                                "failed to persist streaming terminal status/events transition"
                            );
                        }
                    }
                }

                if (durable_status_committed || !persist_status_update)
                    && !persisted_status.is_resumable()
                {
                    Self::schedule_run_eviction(&runs, bg_run_id.clone());
                }

                // Persist usage unconditionally — cancelled runs still consumed tokens
                // and must have accurate usage in durable store for billing/audit.
                astra_core::log_persist!(
                    run_engine
                        .persist_usage(
                            &bg_user_id,
                            &bg_run_id,
                            state.provider_input_tokens(),
                            state.total_completion,
                            state.total_tool_calls,
                        )
                        .await,
                    "run_lifecycle",
                    &bg_run_id,
                    "usage"
                );

                // Persist terminal events to durable store in a single batch.
                if durable_status_committed
                    && persist_streaming_events
                    && !streaming_events_for_durable.is_empty()
                    && !streaming_events_committed
                {
                    match run_engine
                        .append_events_batch(&bg_user_id, &bg_run_id, &streaming_events_for_durable)
                        .await
                    {
                        Ok(()) => {
                            record_durable_run_event_batch_metrics(
                                bg_metrics_registry.as_ref(),
                                "streaming_terminal",
                                "committed",
                                &streaming_events_for_durable,
                            );
                        }
                        Err(error) => {
                            record_durable_run_event_batch_metrics(
                                bg_metrics_registry.as_ref(),
                                "streaming_terminal",
                                "error",
                                &streaming_events_for_durable,
                            );
                            astra_core::agent_warn!(
                                "run_lifecycle",
                                "persist append_streaming_events_batch for run {}: {}",
                                bg_run_id,
                                error
                            );
                        }
                    }
                }

                if let Err(e) = persist_ctx
                    .materialize_run_transcript_evidence(&state)
                    .await
                {
                    tracing::warn!(
                        session_id = %bg_session_id,
                        run_id = %bg_run_id,
                        error = %e,
                        "durable transcript evidence materialization failed"
                    );
                }

                // Keep the owner lease through terminal CAS/event repair, then
                // release it before client fanout and post-loop cleanup. These
                // side effects must not advertise a live resume/input consumer.
                drop(_owner_lease_heartbeat);

                for event in missing_lifecycle_events {
                    if event_tx.send(event).await.is_err() {
                        break;
                    }
                }

                if publish_stream_terminal {
                    for event in streamed_final_events {
                        if event_tx.send(event).await.is_err() {
                            break;
                        }
                    }
                }

                // `turn_complete` carries successful assistant reconciliation data.
                // Failed/cancelled/waiting turns terminate via their run lifecycle
                // event (`run_error`, `run_finished`, `run_waiting`) instead.
                if publish_stream_terminal && should_emit_stream_turn_complete(&persisted_status) {
                    let mut completion_facts =
                        astra_turn_core::complete::TurnCompletionFacts::from_tool_signatures(
                            &state.stall.turn_sigs,
                        );
                    completion_facts.stall_detected |= !state.stall.events.is_empty();
                    let _ = event_tx
                        .send(build_run_turn_complete_event_with_interruption(
                            state.total_tool_calls,
                            &state.final_text,
                            state.interruption.as_ref(),
                            &completion_facts,
                        ))
                        .await;
                }

                // Drop event_tx — signals end-of-stream to the HTTP handler.
                drop(event_tx);

                // Post-loop memory cleanup — identical to `create_run`. Runs
                // AFTER event_tx drops; default async mode schedules external
                // Memoria work without holding the run permit on governance RTT.
                post_loop_memory_cleanup(
                    state.current_session_id.as_deref().unwrap_or(""),
                    &state.session_facts,
                    state.memory_extraction_service.as_ref(),
                    build_shutdown_extraction_request(&state),
                    bg_metrics_registry.clone(),
                )
                .await;

                if let Some(record) = bg_cloud_workspace_record.as_ref() {
                    Self::cleanup_cloud_workspace_after_terminal_run(
                        bg_workspace_record_store,
                        &bg_user_id,
                        &bg_session_id,
                        &bg_run_id,
                        record,
                        &persisted_status,
                    )
                    .await;
                }
            },
            "agentic_loop_stream_chat",
        );

        Ok(ChatStreamRecord {
            session_id,
            run_id,
            events: Vec::new(),
            event_rx: Some(event_rx),
        })
    }

    async fn get_run_status(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
        let run = self.require_durable_run_for_user(&run_id, &user_id).await?;
        Ok(Self::durable_status_record(&run))
    }

    async fn get_run_projection(
        &self,
        run_id: String,
        user_id: String,
        recent_limit: u32,
    ) -> Result<RunProjectionRecord, (StatusCode, Json<ErrorResponse>)> {
        let run = self.require_durable_run_for_user(&run_id, &user_id).await?;
        let projection = self
            .run_engine
            .load_run_projection(&user_id, &run_id)
            .await
            .map_err(|error| {
                error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("Failed to load run projection: {error}"),
                )
            })?;
        let latest_checkpoint = self
            .run_engine
            .load_latest_checkpoint(&user_id, &run_id, None)
            .await
            .map_err(|error| {
                error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("Failed to load run checkpoint: {error}"),
                )
            })?;
        let recent_events = Self::durable_recent_events(&run, recent_limit);
        let binding = Self::durable_run_execution_binding_snapshot(&run);

        if let Some(projection) = projection {
            Ok(RunProjectionRecord {
                run_id: projection.run_id,
                session_id: projection.session_id,
                status: projection.status,
                waiting_for: projection.waiting_for,
                error_message: projection.error_message,
                workspace: binding.workspace.clone(),
                executor: binding.executor.clone(),
                transport: binding.transport.clone(),
                run_event_high_watermark: run.last_event_idx,
                projection_event_idx: projection.projection_event_idx,
                projection_updated_at: projection.updated_at,
                projection_hash: projection.projection_hash,
                latest_event_type: projection.latest_event_type,
                total_prompt_tokens: projection.total_prompt_tokens,
                total_completion_tokens: projection.total_completion_tokens,
                total_tool_calls: projection.total_tool_calls,
                latest_checkpoint: latest_checkpoint.map(|checkpoint| {
                    RunProjectionCheckpointRecord {
                        checkpoint_id: checkpoint.checkpoint_id,
                        checkpoint_kind: checkpoint.checkpoint_kind,
                        checkpoint_version: checkpoint.checkpoint_version,
                        node_seq: checkpoint.node_seq,
                        created_at: checkpoint.created_at,
                    }
                }),
                has_durable_projection: true,
                recent_events,
            })
        } else {
            let latest_event_type = run.events.last().map(astra_services::extract_event_type);
            Ok(RunProjectionRecord {
                run_id: run.run_id.clone(),
                session_id: run.session_id.clone(),
                status: run.status.clone(),
                waiting_for: run.waiting_for.clone(),
                error_message: run.error_message.clone(),
                workspace: binding.workspace,
                executor: binding.executor,
                transport: binding.transport,
                run_event_high_watermark: run.last_event_idx,
                projection_event_idx: run.last_event_idx,
                projection_updated_at: run.updated_at.clone(),
                projection_hash: format!(
                    "{:x}",
                    Sha256::digest(
                        serde_json::json!({
                            "run_id": run.run_id,
                            "status": run.status,
                            "waiting_for": run.waiting_for,
                            "last_event_idx": run.last_event_idx,
                            "total_prompt_tokens": run.total_prompt_tokens,
                            "total_completion_tokens": run.total_completion_tokens,
                            "total_tool_calls": run.total_tool_calls,
                            "latest_event_type": latest_event_type.clone(),
                        })
                        .to_string()
                        .as_bytes()
                    )
                ),
                latest_event_type,
                total_prompt_tokens: run.total_prompt_tokens,
                total_completion_tokens: run.total_completion_tokens,
                total_tool_calls: run.total_tool_calls,
                latest_checkpoint: latest_checkpoint.map(|checkpoint| {
                    RunProjectionCheckpointRecord {
                        checkpoint_id: checkpoint.checkpoint_id,
                        checkpoint_kind: checkpoint.checkpoint_kind,
                        checkpoint_version: checkpoint.checkpoint_version,
                        node_seq: checkpoint.node_seq,
                        created_at: checkpoint.created_at,
                    }
                }),
                has_durable_projection: false,
                recent_events,
            })
        }
    }

    async fn repair_run_projection(
        &self,
        run_id: String,
        user_id: String,
        recent_limit: u32,
    ) -> Result<RunProjectionRecord, (StatusCode, Json<ErrorResponse>)> {
        self.require_durable_run_for_user(&run_id, &user_id).await?;
        let rebuilt = self
            .run_engine
            .rebuild_run_projection(&user_id, &run_id)
            .await
            .map_err(|error| {
                error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("Failed to rebuild run projection: {error}"),
                )
            })?;
        if rebuilt.is_none() {
            return Err(error_response(StatusCode::NOT_FOUND, "Run not found"));
        }
        self.get_run_projection(run_id, user_id, recent_limit).await
    }

    async fn stream_run(
        &self,
        run_id: String,
        user_id: String,
        last_index: u32,
    ) -> Result<Vec<Value>, (StatusCode, Json<ErrorResponse>)> {
        let run = self.require_durable_run_for_user(&run_id, &user_id).await?;
        Ok(Self::durable_stream_events(&run, last_index))
    }

    async fn stream_run_live(
        &self,
        run_id: String,
        user_id: String,
        last_index: u32,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
        let durable = self.require_durable_run_for_user(&run_id, &user_id).await?;
        let live_tx = {
            let runs = self.runs.read().await;
            runs.get(&run_id).and_then(|run| run.live_tx.clone())
        };
        if let Some(live_tx) = live_tx {
            let replay_events = Self::durable_stream_events(&durable, last_index);
            let mut live_rx = live_tx.subscribe();
            let (event_tx, event_rx) = mpsc::channel(512);
            spawn_observed(
                async move {
                    for event in replay_events {
                        if event_tx.send(event).await.is_err() {
                            return;
                        }
                    }
                    loop {
                        match live_rx.recv().await {
                            Ok(event) => {
                                if event_tx.send(event).await.is_err() {
                                    return;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => return,
                        }
                    }
                },
                "durable_stream_replay",
            );
            return Ok(ChatStreamRecord {
                session_id: durable.session_id,
                run_id,
                events: Vec::new(),
                event_rx: Some(event_rx),
            });
        }

        let events = Self::durable_stream_events(&durable, last_index);
        Ok(ChatStreamRecord {
            session_id: durable.session_id,
            run_id,
            events,
            event_rx: None,
        })
    }

    async fn drain_approval_requests(&self, run_id: &str) -> Vec<serde_json::Value> {
        let mut channels = self.approval_channels.lock().await;
        let Some(rx) = channels.get_mut(run_id) else {
            return vec![];
        };
        let mut requests = Vec::new();
        while let Ok(req) = rx.try_recv() {
            requests.push(req);
        }
        requests
    }

    async fn drain_user_prompt_requests(&self, run_id: &str) -> Vec<serde_json::Value> {
        let mut channels = self.user_prompt_channels.lock().await;
        let Some(rx) = channels.get_mut(run_id) else {
            return vec![];
        };
        let mut requests = Vec::new();
        while let Ok(req) = rx.try_recv() {
            requests.push(req);
        }
        requests
    }

    async fn get_run_interaction_event(
        &self,
        run_id: String,
        user_id: String,
        request_id: String,
        event_type: String,
    ) -> Result<Option<Value>, (StatusCode, Json<ErrorResponse>)> {
        self.run_engine
            .load_run_interaction_event(&user_id, &run_id, &request_id, &event_type)
            .await
            .map_err(|error| error_response(StatusCode::SERVICE_UNAVAILABLE, error))
    }

    async fn resolve_run_interaction(
        &self,
        run_id: String,
        user_id: String,
        request_id: String,
        kind: astra_services::runs::DurableRunInteractionKind,
        response_data: Value,
    ) -> Result<
        astra_services::runs::DurableRunInteractionResolveOutcome,
        (StatusCode, Json<ErrorResponse>),
    > {
        self.run_engine
            .resolve_run_interaction(&user_id, &run_id, &request_id, kind, response_data)
            .await
            .map_err(|error| error_response(StatusCode::SERVICE_UNAVAILABLE, error))
    }

    async fn drain_progress_events(&self, run_id: &str) -> Vec<serde_json::Value> {
        let mut channels = self.progress_channels.lock().await;
        let Some(rx) = channels.get_mut(run_id) else {
            return vec![];
        };
        let mut events = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            events.push(serde_json::to_value(&evt).unwrap_or_default());
        }
        events
    }

    async fn submit_run_user_intent(
        &self,
        run_id: String,
        user_id: String,
        input: RunUserIntentData,
    ) -> Result<RunUserIntentRecord, (StatusCode, Json<ErrorResponse>)> {
        if input.intent_id.trim().is_empty() {
            return Err(error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "intent_id is required",
            ));
        }
        if crate::turn::run_control::user_intent_content(&input.input).is_none() {
            return Err(error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "user intent must contain actionable content",
            ));
        }
        if user_intent_text_len(&input.input) > MAX_USER_INTENT_CHARS {
            return Err(error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "user intent is too large",
            ));
        }

        let intent_id = input.intent_id.trim().to_string();
        let event = json!({
            "event_type": "user_intent",
            "idempotency_key": format!("user_intent:{intent_id}"),
            "data": {
                "intent_id": intent_id,
                "delivery": input.delivery,
                "input": input.input,
            },
        });

        // A run may move between active execution states while this request is
        // being accepted. Retry only that optimistic storage race; never turn
        // an intent into a run-state transition or overwrite newer state.
        for _ in 0..3 {
            let durable = self.require_durable_run_for_user(&run_id, &user_id).await?;
            let durable_status = Self::run_status_from_durable(&durable.status)?;
            if matches!(
                durable_status,
                RunStatus::Paused
                    | RunStatus::Completed
                    | RunStatus::Delegated
                    | RunStatus::Failed
                    | RunStatus::Cancelled
            ) {
                return Err(Self::run_state_conflict("submit input to", &durable.status));
            }

            let duplicate = durable.events.iter().any(|event| {
                event
                    .get("data")
                    .and_then(|data| data.get("intent_id"))
                    .and_then(Value::as_str)
                    == Some(intent_id.as_str())
            });
            if duplicate {
                return Ok(RunUserIntentRecord {
                    run_id,
                    intent_id,
                    status: UserIntentStatus::AcceptedRemote,
                    duplicate: true,
                });
            }

            if !self.run_execution_is_live(&durable).await {
                self.reconcile_orphaned_execution_for_session_continuation(
                    &durable,
                    "submit_user_intent",
                )
                .await?;
                return Err(error_response_coded(
                    StatusCode::CONFLICT,
                    "This run no longer has a live input consumer. Continue the session to start a new run instead of queueing input to an orphaned execution.",
                    "run_intent_consumer_not_live",
                ));
            }

            match self
                .run_engine
                .transition_status_with_events_if_current(
                    &user_id,
                    &run_id,
                    &[durable.status.as_str()],
                    &durable.status,
                    durable.waiting_for.as_deref(),
                    durable.error_message.as_deref(),
                    std::slice::from_ref(&event),
                )
                .await
            {
                Ok(true) => {
                    let mut stream_intent_event = event.clone();
                    if let Some(obj) = stream_intent_event.as_object_mut() {
                        obj.insert(
                            "index".to_string(),
                            json!(durable.last_event_idx.saturating_add(1)),
                        );
                    }
                    let live_events = run_handlers::transform_stream_run_events_for_client(
                        &run_id,
                        vec![stream_intent_event],
                    );
                    let live_tx = if let Some(run) = self.runs.write().await.get_mut(&run_id) {
                        run.events.push(event.clone());
                        run.live_tx.clone()
                    } else {
                        None
                    };
                    if let Some(live_tx) = live_tx {
                        for event in live_events {
                            if live_tx.send(event).is_err() {
                                tracing::warn!(
                                    target: "astra_runtime::lifecycle",
                                    run_id = %run_id,
                                    "live event channel closed; event fanout may be incomplete"
                                );
                                break;
                            }
                        }
                    }
                    return Ok(RunUserIntentRecord {
                        run_id,
                        intent_id,
                        status: UserIntentStatus::AcceptedRemote,
                        duplicate: false,
                    });
                }
                Ok(false) => continue,
                Err(error) => {
                    let durable_after_error =
                        self.require_durable_run_for_user(&run_id, &user_id).await?;
                    let duplicate_committed = durable_after_error.events.iter().any(|event| {
                        event
                            .get("data")
                            .and_then(|data| data.get("intent_id"))
                            .and_then(Value::as_str)
                            == Some(intent_id.as_str())
                    });
                    if duplicate_committed {
                        return Ok(RunUserIntentRecord {
                            run_id,
                            intent_id,
                            status: UserIntentStatus::AcceptedRemote,
                            duplicate: true,
                        });
                    }
                    return Err(Self::durable_persist_error("user intent append", error));
                }
            }
        }

        let durable_after_conflict = self.require_durable_run_for_user(&run_id, &user_id).await?;
        Err(Self::run_state_conflict(
            "submit input to",
            &durable_after_conflict.status,
        ))
    }

    async fn drain_background_tasks(&self, timeout: std::time::Duration) -> bool {
        self.drain_background_tasks_impl(timeout).await
    }

    async fn cancel_run(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
        let durable = self.require_durable_run_for_user(&run_id, &user_id).await?;
        let durable_status = Self::run_status_from_durable(&durable.status)?;
        let Some(cancelled_status) = durable_status.control_action_target(RunControlAction::Cancel)
        else {
            if durable_status == RunStatus::Cancelled {
                self.converge_cancelled_run_descendants(&user_id, &durable.session_id, &run_id)
                    .await;
            }
            return Ok(CancelRunRecord {
                run_id,
                status: durable.status,
            });
        };

        let cancel_event = json!({"event_type": "run_finished", "data": {"cancelled": true}});
        let status_updated = self
            .run_engine
            .transition_status_with_event_if_current(
                &user_id,
                &run_id,
                &[durable_status.as_str()],
                cancelled_status.as_str(),
                None,
                None,
                cancel_event.clone(),
            )
            .await
            .map_err(|error| Self::durable_persist_error("cancel transition", error))?;
        if !status_updated {
            let current = self.require_durable_run_for_user(&run_id, &user_id).await?;
            let current_status = Self::run_status_from_durable(&current.status)?;
            if matches!(
                current_status,
                RunStatus::Completed
                    | RunStatus::Delegated
                    | RunStatus::Failed
                    | RunStatus::Cancelled
            ) {
                if current_status == RunStatus::Cancelled {
                    self.converge_cancelled_run_descendants(&user_id, &current.session_id, &run_id)
                        .await;
                }
                return Ok(CancelRunRecord {
                    run_id,
                    status: current.status,
                });
            }
            return Err(Self::run_state_conflict("cancel", &current.status));
        }

        {
            let mut runs = self.runs.write().await;
            if let Some(run) = runs.get_mut(&run_id) {
                run.cancel_flag.store(true, Ordering::SeqCst);
                run.pause_flag.store(false, Ordering::SeqCst);
                run.llm_cancel_token.cancel();
                run.status = cancelled_status;
                run.waiting_for = None;
                run.events.push(cancel_event);
            }
        }

        self.converge_cancelled_run_descendants(&user_id, &durable.session_id, &run_id)
            .await;
        Self::schedule_run_eviction(&self.runs, run_id.clone());

        Ok(CancelRunRecord {
            run_id,
            status: cancelled_status.as_str().to_string(),
        })
    }

    async fn cancel_session_runs(
        &self,
        session_id: String,
        user_id: String,
    ) -> Result<Vec<CancelRunRecord>, (StatusCode, Json<ErrorResponse>)> {
        let mut cancelled = Vec::new();
        let mut seen = HashSet::new();
        loop {
            let Some(run) = self
                .run_engine
                .find_blocking_session_run(&user_id, &session_id)
                .await
                .map_err(|error| {
                    error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        format!("Failed to find blocking session run: {error}"),
                    )
                })?
            else {
                break;
            };

            if !seen.insert(run.run_id.clone()) {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    format!(
                        "session cancel made no progress while cancelling run {}",
                        run.run_id
                    ),
                ));
            }
            cancelled.push(self.cancel_run(run.run_id, user_id.clone()).await?);
        }
        Ok(cancelled)
    }

    async fn list_runs_cursor(
        &self,
        user_id: String,
        limit: u32,
        cursor: Option<RunListCursor>,
    ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
        let limit = astra_services::runs::validate_run_list_limit(limit);
        let durable_page = self
            .run_engine
            .list_user_runs_cursor(&user_id, limit, cursor)
            .await
            .map_err(|error| {
                error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("Failed to list durable run state: {error}"),
                )
            })?;
        let page = durable_page
            .runs
            .iter()
            .map(Self::durable_status_record)
            .collect();
        Ok(RunListRecord {
            runs: page,
            total: durable_page.total,
            limit,
            next_cursor: durable_page.next_cursor,
        })
    }

    async fn list_session_runs(
        &self,
        user_id: String,
        session_id: String,
        limit: u32,
    ) -> Result<astra_services::runs::DurableSessionRunPage, (StatusCode, Json<ErrorResponse>)>
    {
        self.run_engine
            .list_session_runs(&user_id, &session_id, limit)
            .await
            .map_err(|error| {
                error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("Failed to list durable session runs: {error}"),
                )
            })
    }

    async fn pause_run(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<RunMutationRecord, (StatusCode, Json<ErrorResponse>)> {
        let durable = self.require_durable_run_for_user(&run_id, &user_id).await?;
        let durable_status = Self::run_status_from_durable(&durable.status)?;
        let Some(paused_status) = durable_status.control_action_target(RunControlAction::Pause)
        else {
            return Err(Self::run_state_conflict("pause", &durable.status));
        };

        if !self.run_execution_is_live(&durable).await {
            if self
                .reconcile_orphaned_execution_for_session_continuation(&durable, "pause")
                .await?
            {
                return Ok(RunMutationRecord::applied(
                    run_id,
                    STATUS_PAUSED,
                    durable.status,
                ));
            }
            let current = self.require_durable_run_for_user(&run_id, &user_id).await?;
            return Err(Self::run_state_conflict("pause", &current.status));
        }

        let pause_event = json!({"event_type": "run_paused", "data": {}});
        // Always write to DB first — the source of truth for cross-pod control.
        let status_updated = self
            .run_engine
            .transition_status_with_event_if_current(
                &user_id,
                &run_id,
                &[durable_status.as_str()],
                paused_status.as_str(),
                Some("user_resume"),
                None,
                pause_event.clone(),
            )
            .await
            .map_err(|error| Self::durable_persist_error("pause transition", error))?;
        if !status_updated {
            let current = self.require_durable_run_for_user(&run_id, &user_id).await?;
            return Err(Self::run_state_conflict("pause", &current.status));
        }

        {
            let mut runs = self.runs.write().await;
            if let Some(run) = runs.get_mut(&run_id) {
                run.pause_flag.store(true, Ordering::SeqCst);
                run.status = paused_status;
                run.waiting_for = Some("user_resume".to_string());
                run.events.push(pause_event);
            }
        }
        if let Some(de) = &self.delegation_engine {
            de.pause_children_of(&user_id, &run_id).await;
        }
        Ok(RunMutationRecord::applied(
            run_id,
            paused_status.as_str(),
            durable.status,
        ))
    }

    async fn resume_run(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<RunMutationRecord, (StatusCode, Json<ErrorResponse>)> {
        let durable = self.require_durable_run_for_user(&run_id, &user_id).await?;
        let durable_status = Self::run_status_from_durable(&durable.status)?;
        let Some(running_status) = durable_status.control_action_target(RunControlAction::Resume)
        else {
            return Err(Self::run_state_conflict("resume", &durable.status));
        };

        // Buffered completion resume is completion promotion, not execution
        // resumption. It deliberately bypasses the session execution slot so a
        // paused run with an already-buffered terminal answer can be finalized
        // even if another root turn later acquired the session slot.
        if has_buffered_terminal_completion(&durable.events) {
            let status_updated = self
                .run_engine
                .persist_status_if_current(
                    &user_id,
                    &run_id,
                    &[durable_status.as_str()],
                    STATUS_COMPLETED,
                    None,
                    None,
                )
                .await
                .map_err(|error| Self::durable_persist_error("resume completed status", error))?;
            if !status_updated {
                let current = self.require_durable_run_for_user(&run_id, &user_id).await?;
                let current_status = Self::run_status_from_durable(&current.status)?;
                if matches!(
                    current_status,
                    RunStatus::Completed
                        | RunStatus::Delegated
                        | RunStatus::Failed
                        | RunStatus::Cancelled
                ) {
                    return Ok(RunMutationRecord::applied(
                        run_id,
                        current.status,
                        durable.status,
                    ));
                }
                return Err(Self::run_state_conflict("resume", &current.status));
            }
            {
                let mut runs = self.runs.write().await;
                if let Some(run) = runs.get_mut(&run_id) {
                    run.status = RunStatus::Completed;
                    run.pause_flag.store(false, Ordering::SeqCst);
                    run.waiting_for = None;
                    run.live_tx = None;
                }
            }
            Self::schedule_run_eviction(&self.runs, run_id.clone());
            return Ok(RunMutationRecord::applied(
                run_id,
                STATUS_COMPLETED,
                durable.status,
            ));
        }

        if !self.run_execution_is_live(&durable).await {
            self.reconcile_orphaned_execution_for_session_continuation(&durable, "resume")
                .await?;
            if self
                .run_engine
                .find_blocking_session_run(&user_id, &durable.session_id)
                .await
                .map_err(|error| {
                    error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        format!("Failed to verify session execution slot: {error}"),
                    )
                })?
                .is_some_and(|blocker| blocker.run_id != run_id)
            {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "session already has an active run".to_string(),
                ));
            }
            let current = self.require_durable_run_for_user(&run_id, &user_id).await?;
            if Self::run_status_from_durable(&current.status)? != durable_status {
                return Err(Self::run_state_conflict("resume", &current.status));
            }
            return Ok(RunMutationRecord {
                run_id: run_id.clone(),
                status: current.status,
                previous_status: durable.status,
                disposition: RunMutationDisposition::SessionContinuationRequired,
                continuation: Some(RunContinuationRecord {
                    strategy: "session_continuation".into(),
                    session_id: durable.session_id,
                    source_run_id: run_id,
                }),
            });
        }

        let resume_event = json!({"event_type": "run_resumed", "data": {}});
        // Always write to DB first — the source of truth for cross-pod control.
        let transition = self
            .run_engine
            .transition_status_with_event_if_current_unless_session_blocked(
                &user_id,
                &run_id,
                &durable.session_id,
                &[durable_status.as_str()],
                running_status.as_str(),
                None,
                None,
                resume_event.clone(),
            )
            .await
            .map_err(|error| Self::durable_persist_error("resume transition", error))?;
        match transition {
            astra_services::runs::GuardedRunStatusTransition::Updated => {}
            astra_services::runs::GuardedRunStatusTransition::SessionBlocked => {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "session already has an active run".to_string(),
                ));
            }
            astra_services::runs::GuardedRunStatusTransition::StatusConflict => {
                let current = self.require_durable_run_for_user(&run_id, &user_id).await?;
                return Err(Self::run_state_conflict("resume", &current.status));
            }
        }

        {
            let mut runs = self.runs.write().await;
            if let Some(run) = runs.get_mut(&run_id) {
                run.pause_flag.store(false, Ordering::SeqCst);
                run.status = running_status;
                run.waiting_for = None;
                run.events.push(resume_event);
            }
        }
        if let Some(de) = &self.delegation_engine {
            de.resume_children_of(&user_id, &run_id).await;
        }
        Ok(RunMutationRecord::applied(
            run_id,
            running_status.as_str(),
            durable.status,
        ))
    }
}

// ─── Sub-Run Executor ───────────────────────────────────────────────────────

use crate::server::delegation::engine::{SubRunConfig, SubRunExecutor};

/// Server-side executor for dynamic `agent(action='spawn')` children.
///
/// It reuses the production sub-run loop executor so Web dynamic agents run
/// with the same server host, tool backend, skill resolver, memory plumbing,
/// and observe-only harness path as delegated children. Spawn-specific
/// semantics stay in `DynamicAgentSpawner` and `agent_tool`.
pub struct ServerSpawnAgentExecutor {
    matrixone: MatrixOneSettings,
    encryptor: Arc<FernetTokenEncryptor>,
    /// The session lifecycle's authoritative run engine. Dynamic sub-runs must
    /// not reconstruct this from a pool: doing so creates a fresh owner-pod
    /// identity and splits lease/control authority from their parent run.
    run_engine: Option<RunEngine>,
    shared_pool: Option<SharedPool>,
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    edge_connection_pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    edge_dispatch_service: Option<Arc<dyn astra_services::multi_agent::EdgeDispatchService>>,
    edge_registry_service: Option<Arc<dyn astra_services::multi_agent::EdgeRegistryService>>,
    skill_service: Option<Arc<dyn SkillService>>,
    memory_extraction_service: Option<Arc<crate::session_memory::MemoryExtractionService>>,
    reflect_service: Arc<dyn astra_services::ReflectService>,
    runtime_contexts: Arc<RwLock<HashMap<String, ServerSpawnRuntimeContext>>>,
}

impl ServerSpawnAgentExecutor {
    pub fn new(
        matrixone: MatrixOneSettings,
        encryptor: Arc<FernetTokenEncryptor>,
        edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    ) -> Self {
        Self {
            matrixone,
            encryptor,
            run_engine: None,
            shared_pool: None,
            edge_callback_ledger,
            edge_connection_pool: None,
            edge_dispatch_service: None,
            edge_registry_service: None,
            skill_service: None,
            memory_extraction_service: None,
            reflect_service: Arc::new(astra_services::UnconfiguredReflectService),
            runtime_contexts: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn with_run_engine(mut self, run_engine: RunEngine) -> Self {
        self.run_engine = Some(run_engine);
        self
    }

    pub fn with_pool(mut self, pool: Option<SharedPool>) -> Self {
        self.shared_pool = pool;
        self
    }

    pub fn with_edge_connection_pool(
        mut self,
        pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    ) -> Self {
        self.edge_connection_pool = pool;
        self
    }

    pub fn with_edge_dispatch_service(
        mut self,
        svc: Arc<dyn astra_services::multi_agent::EdgeDispatchService>,
    ) -> Self {
        self.edge_dispatch_service = Some(svc);
        self
    }

    pub fn with_edge_registry_service(
        mut self,
        svc: Arc<dyn astra_services::multi_agent::EdgeRegistryService>,
    ) -> Self {
        self.edge_registry_service = Some(svc);
        self
    }

    pub fn with_skill_service(mut self, service: Option<Arc<dyn SkillService>>) -> Self {
        self.skill_service = service;
        self
    }

    pub fn with_memory_extraction_service(
        mut self,
        svc: Option<Arc<crate::session_memory::MemoryExtractionService>>,
    ) -> Self {
        self.memory_extraction_service = svc;
        self
    }

    pub fn with_reflect_service(
        mut self,
        service: Arc<dyn astra_services::ReflectService>,
    ) -> Self {
        self.reflect_service = service;
        self
    }

    async fn set_runtime_context(&self, context: ServerSpawnRuntimeContext) {
        self.runtime_contexts
            .write()
            .await
            .insert(context.parent_run_id.clone(), context);
    }

    async fn remove_runtime_context(&self, run_id: &str) {
        self.runtime_contexts.write().await.remove(run_id);
    }

    async fn runtime_context_for_config(
        &self,
        config: &SpawnRunConfig,
    ) -> Result<ServerSpawnRuntimeContext, String> {
        let parent_run_id = config
            .parent_address
            .as_ref()
            .map(|address| address.run_id.as_str())
            .ok_or_else(|| {
                "server dynamic agent executor requires parent run lineage".to_string()
            })?;

        self.runtime_contexts
            .read()
            .await
            .get(parent_run_id)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "server dynamic agent executor has no runtime context for parent run {parent_run_id}"
                )
            })
    }

    /// Publish the child as the next possible parent before its loop starts.
    ///
    /// Dynamic agents use the same session-owned spawner at every depth.  A
    /// child therefore needs its own runtime context keyed by *its* run id,
    /// otherwise a later `agent(action='spawn')` from that child can only
    /// find the original root context and fails before a grandchild begins.
    /// This is execution lineage, not prompt context; the inherited request
    /// constraints are narrowed once here and carried to all descendants.
    async fn register_child_runtime_context(
        &self,
        parent: &ServerSpawnRuntimeContext,
        config: &SpawnRunConfig,
        request_constraints: RequestConstraints,
    ) -> ServerSpawnRuntimeContext {
        // A child owns its own local control handles. Parent cancellation is
        // inherited through the token tree, while a child's direct pause or
        // cancellation can never clear or otherwise mutate its parent.
        let pause_flag = Arc::new(AtomicBool::new(false));
        let cancel_token = Arc::new(
            parent
                .cancel_token
                .as_ref()
                .map(|token| token.child_token())
                .unwrap_or_default(),
        );
        let child_context = ServerSpawnRuntimeContext {
            parent_run_id: config.run_id.clone(),
            user_id: parent.user_id.clone(),
            session_id: parent.session_id.clone(),
            trace_context: parent.trace_context.clone(),
            forward_headers: parent.forward_headers.clone(),
            admitted_model_execution: parent.admitted_model_execution.clone(),
            request_constraints,
            execution_metadata: config
                .execution_metadata
                .clone()
                .or_else(|| parent.execution_metadata.clone()),
            spawner: parent.spawner.clone(),
            pause_flag: Some(pause_flag),
            cancel_token: Some(cancel_token),
            #[cfg(feature = "bridge-e2e-hooks")]
            test_child_llm_rounds: parent.test_child_llm_rounds.clone(),
            #[cfg(feature = "harness")]
            harness_sink: parent.harness_sink.clone(),
        };
        self.set_runtime_context(child_context.clone()).await;
        child_context
    }

    fn build_subrun_executor(
        &self,
        inherited_permissions: InheritedPermissions,
        dynamic_agent_spawner: Arc<DynamicAgentSpawner>,
        client_tool_delivery_tx: Option<mpsc::Sender<Value>>,
        admitted_model_execution: Option<&astra_services::AdmittedModelExecution>,
    ) -> ServerSubRunExecutor {
        let mut executor = ServerSubRunExecutor::new(
            self.matrixone.clone(),
            Arc::clone(&self.encryptor),
            Arc::clone(&self.edge_callback_ledger),
        );
        if let Some(run_engine) = self.run_engine.clone() {
            executor = executor.with_run_engine(run_engine);
        }
        executor = executor.with_inherited_permissions(inherited_permissions);
        if let Some(pool) = self.shared_pool.clone() {
            executor = executor.with_pool(pool);
        }
        if let Some(pool) = self.edge_connection_pool.clone() {
            executor = executor.with_edge_connection_pool(pool);
        }
        if let Some(svc) = self.edge_dispatch_service.clone() {
            executor = executor.with_edge_dispatch_service(svc);
        }
        if let Some(svc) = self.edge_registry_service.clone() {
            executor = executor.with_edge_registry_service(svc);
        }
        if let Some(service) = self.skill_service.clone() {
            executor = executor.with_skill_service(service);
        }
        if let Some(svc) = self.memory_extraction_service.clone() {
            executor = executor.with_memory_extraction_service(svc);
        }
        executor = executor
            .with_admitted_model_execution(admitted_model_execution.cloned())
            .with_reflect_service(Arc::clone(&self.reflect_service))
            .with_dynamic_agent_spawner(dynamic_agent_spawner)
            .with_client_tool_delivery_tx(client_tool_delivery_tx);
        executor
    }
}

fn spawn_child_request_constraints(
    parent: &RequestConstraints,
    config: &SpawnRunConfig,
) -> RequestConstraints {
    let child_allowed = if config.allowed_tools.iter().any(|tool| tool == "*") {
        if config.read_only {
            Some(
                ["bash", "glob", "grep", "list_dir", "read_file"]
                    .into_iter()
                    .map(String::from)
                    .collect::<HashSet<_>>(),
            )
        } else {
            None
        }
    } else {
        Some(
            config
                .allowed_tools
                .iter()
                .map(|tool| tool.trim().to_ascii_lowercase())
                .filter(|tool| !tool.is_empty())
                .collect::<HashSet<_>>(),
        )
    };

    let allowed_tools = match (&parent.allowed_tools, child_allowed) {
        (Some(parent), Some(child)) => Some(parent.intersection(&child).cloned().collect()),
        (Some(parent), None) => Some(parent.clone()),
        (None, Some(child)) => Some(child),
        (None, None) => None,
    };

    RequestConstraints::new(
        allowed_tools,
        parent.enabled_tools.clone(),
        parent.allowed_skills.clone(),
        parent.allowed_skill_sources.clone(),
    )
}

fn spawn_system_prompt(config: &SpawnRunConfig) -> String {
    if config.system_prompt_addendum.trim().is_empty() {
        format!(
            "You are '{}', a specialized sub-agent. Complete the task thoroughly.",
            config.agent_id
        )
    } else {
        format!(
            "You are '{}', a specialized sub-agent.\n\n{}\n\nComplete the task thoroughly.",
            config.agent_id, config.system_prompt_addendum
        )
    }
}

fn emit_server_subrun_agent_terminated(
    sink: Option<&SharedAgentLiveEventSink>,
    run_id: &str,
    agent_id: &str,
    started_at: Instant,
    termination: AgentLiveTermination,
    reason: Option<String>,
) {
    let Some(sink) = sink else {
        return;
    };
    if let Err(error) = sink.send(AgentLiveEvent {
        run_id: run_id.to_string(),
        agent_id: agent_id.to_string(),
        kind: AgentLiveEventKind::AgentTerminated {
            termination,
            duration_ms: started_at.elapsed().as_millis() as u64,
            reason,
        },
    }) {
        tracing::warn!(
            target: "astra_runtime::work_surface",
            agent_id,
            error = ?error,
            "failed to emit server subrun terminal live event"
        );
    }
}

fn emit_server_subrun_execution_waiting(
    sink: Option<&SharedAgentLiveEventSink>,
    run_id: &str,
    agent_id: &str,
    reason: String,
) {
    let Some(sink) = sink else {
        return;
    };
    if let Err(error) = sink.send(AgentLiveEvent {
        run_id: run_id.to_string(),
        agent_id: agent_id.to_string(),
        kind: AgentLiveEventKind::Signal(AgentLiveSignal::ExecutionWaiting { reason }),
    }) {
        tracing::warn!(
            target: "astra_runtime::work_surface",
            agent_id,
            error = ?error,
            "failed to emit server subrun waiting live event"
        );
    }
}

fn emit_server_subrun_transcript_committed(
    sink: Option<&SharedAgentLiveEventSink>,
    run_id: &str,
    agent_id: &str,
    source_event_id: String,
) {
    let Some(sink) = sink else {
        return;
    };
    if let Err(error) = sink.send(AgentLiveEvent {
        run_id: run_id.to_string(),
        agent_id: agent_id.to_string(),
        kind: AgentLiveEventKind::Signal(AgentLiveSignal::TranscriptCommitted {
            source_event_id,
            transcript_location: AgentTranscriptLocation::DurableServer,
        }),
    }) {
        tracing::debug!(
            target: "astra_runtime::work_surface",
            agent_id,
            ?error,
            "server transcript commit was not delivered to the live workbench"
        );
    }
}

fn server_subrun_live_termination(
    outcome: &Result<AgenticLoopOutcome, astra_core::ClassifiedError>,
    loop_state: &AgenticLoopState,
) -> Option<AgentLiveTermination> {
    match outcome {
        Ok(AgenticLoopOutcome::Completed) => {
            match server_subrun_completed_agent_status(loop_state) {
                STATUS_COMPLETED => Some(AgentLiveTermination::Completed),
                STATUS_PAUSED | astra_services::coordination::AGENT_RESULT_STATUS_PARTIAL => {
                    Some(AgentLiveTermination::Interrupted)
                }
                _ => Some(AgentLiveTermination::Failed),
            }
        }
        Ok(AgenticLoopOutcome::Cancelled) => Some(AgentLiveTermination::Cancelled),
        Ok(AgenticLoopOutcome::Waiting(_)) => None,
        Ok(AgenticLoopOutcome::Delegated) => Some(AgentLiveTermination::Delegated),
        Ok(AgenticLoopOutcome::Error(_) | AgenticLoopOutcome::ControlRejected(_)) | Err(_) => {
            Some(AgentLiveTermination::Failed)
        }
    }
}

fn server_subrun_live_reason(
    outcome: &Result<AgenticLoopOutcome, astra_core::ClassifiedError>,
    loop_state: &AgenticLoopState,
) -> Option<String> {
    match outcome {
        Ok(AgenticLoopOutcome::Completed) if loop_state.interruption.is_some() => loop_state
            .interruption
            .as_ref()
            .map(|interruption| interruption.kind.label().to_string()),
        Ok(AgenticLoopOutcome::Completed) => None,
        Ok(AgenticLoopOutcome::Delegated) => Some("delegated".to_string()),
        Ok(AgenticLoopOutcome::Cancelled) => Some("cancelled".to_string()),
        Ok(AgenticLoopOutcome::Waiting(reason)) => Some(reason.clone()),
        Ok(AgenticLoopOutcome::Error(error)) => Some(error.clone()),
        Ok(AgenticLoopOutcome::ControlRejected(rejection)) => {
            Some(format!("{}: {}", rejection.code, rejection.message))
        }
        Err(error) => Some(error.to_string()),
    }
}

fn server_subrun_outcome_status(
    outcome: &Result<AgenticLoopOutcome, astra_core::ClassifiedError>,
    loop_state: &AgenticLoopState,
) -> &'static str {
    match outcome {
        Ok(AgenticLoopOutcome::Completed) => server_subrun_completed_agent_status(loop_state),
        Ok(AgenticLoopOutcome::Delegated) => STATUS_DELEGATED,
        Ok(AgenticLoopOutcome::Cancelled) => STATUS_CANCELLED,
        Ok(AgenticLoopOutcome::Waiting(_)) => STATUS_WAITING,
        Ok(AgenticLoopOutcome::Error(_) | AgenticLoopOutcome::ControlRejected(_)) | Err(_) => {
            STATUS_FAILED
        }
    }
}

/// Project a loop-level `Completed` outcome into the richer child-result
/// taxonomy. Structured interruptions are not all pauses: an immediately
/// resumable cutoff is terminal partial evidence, while an explicit recovery
/// action remains a durable execution hold.
fn server_subrun_completed_agent_status(loop_state: &AgenticLoopState) -> &'static str {
    let Some(interruption) = loop_state.interruption.as_ref() else {
        return STATUS_COMPLETED;
    };
    if interruption.kind == InterruptionKind::HarnessPaused {
        return STATUS_PAUSED;
    }
    match interruption.resume_action {
        ResumeAction::ContinueImmediately => {
            astra_services::coordination::AGENT_RESULT_STATUS_PARTIAL
        }
        ResumeAction::WaitAndRetry { .. }
        | ResumeAction::RequiresIntervention { .. }
        | ResumeAction::CompactAndRetry => STATUS_PAUSED,
        ResumeAction::StartNewSession => STATUS_FAILED,
    }
}

fn server_subrun_durable_status(
    outcome: &Result<AgenticLoopOutcome, astra_core::ClassifiedError>,
    loop_state: &AgenticLoopState,
) -> &'static str {
    // The durable run lifecycle intentionally has no `partial` state. A
    // partial child result is terminal and therefore stored as failed while
    // the richer AgentResult retains `partial`, output, and interruption
    // reason for parent aggregation and UI projection.
    match server_subrun_outcome_status(outcome, loop_state) {
        astra_services::coordination::AGENT_RESULT_STATUS_PARTIAL => STATUS_FAILED,
        status => status,
    }
}

fn server_subrun_interruption_reason(loop_state: &AgenticLoopState) -> Option<String> {
    loop_state.interruption.as_ref().map(|interruption| {
        let label = interruption.kind.label();
        match interruption.error_detail.as_deref() {
            Some(detail) if !detail.trim().is_empty() => format!("{label}: {detail}"),
            _ => label.to_string(),
        }
    })
}

fn server_subrun_durable_error(
    outcome: &Result<AgenticLoopOutcome, astra_core::ClassifiedError>,
    agent_status: &str,
    interruption_reason: Option<&str>,
) -> Option<String> {
    match outcome {
        Ok(AgenticLoopOutcome::Error(error)) => Some(error.clone()),
        Ok(AgenticLoopOutcome::ControlRejected(rejection)) => {
            Some(format!("{}: {}", rejection.code, rejection.message))
        }
        Err(error) => Some(error.to_string()),
        Ok(AgenticLoopOutcome::Completed)
            if agent_status == astra_services::coordination::AGENT_RESULT_STATUS_PARTIAL =>
        {
            interruption_reason.map(ToString::to_string)
        }
        Ok(AgenticLoopOutcome::Completed) if agent_status == STATUS_FAILED => {
            interruption_reason.map(ToString::to_string)
        }
        _ => None,
    }
}

fn server_subrun_durable_error_code(agent_status: &str) -> Option<&'static str> {
    (agent_status == astra_services::coordination::AGENT_RESULT_STATUS_PARTIAL)
        .then_some(astra_services::coordination::AGENT_RESULT_PARTIAL_DURABLE_ERROR_CODE)
}

fn server_subrun_waiting_for<'a>(
    outcome: &'a Result<AgenticLoopOutcome, astra_core::ClassifiedError>,
    durable_status: &str,
    interruption_reason: Option<&'a str>,
) -> Option<&'a str> {
    match outcome {
        Ok(AgenticLoopOutcome::Waiting(reason)) => Some(reason.as_str()),
        Ok(AgenticLoopOutcome::Completed) if durable_status == STATUS_PAUSED => interruption_reason,
        _ => None,
    }
}

#[async_trait]
impl SpawnAgentExecutor for ServerSpawnAgentExecutor {
    async fn cancel_spawned_run(
        &self,
        run_id: &str,
        user_id: Option<&str>,
        reason: &str,
    ) -> Result<(), String> {
        let runtime_context = self.runtime_contexts.read().await.get(run_id).cloned();
        if let Some(token) = runtime_context
            .as_ref()
            .and_then(|context| context.cancel_token.as_ref())
        {
            token.cancel();
        }

        let resolved_user_id = user_id.map(str::to_string).or_else(|| {
            runtime_context
                .as_ref()
                .map(|context| context.user_id.clone())
        });
        if let (Some(run_engine), Some(user_id)) =
            (self.run_engine.as_ref(), resolved_user_id.as_deref())
        {
            let event = json!({
                "event_type": "run_finished",
                "data": {
                    "cancelled": true,
                    "reason": reason,
                    "source": "ancestor_run",
                }
            });
            run_engine
                .transition_status_with_event_if_current(
                    user_id,
                    run_id,
                    &[STATUS_RUNNING, STATUS_WAITING, STATUS_PAUSED],
                    STATUS_CANCELLED,
                    None,
                    None,
                    event,
                )
                .await?;
        }
        self.remove_runtime_context(run_id).await;
        Ok(())
    }

    async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
        let context = self.runtime_context_for_config(&config).await?;
        let dynamic_agent_spawner = context.spawner.upgrade().ok_or_else(|| {
            "server dynamic agent lifecycle is no longer available for this session".to_string()
        })?;
        // The child executor runs inside this spawner's own JoinSet. Give its
        // nested-agent tool context a non-owning handle so the future cannot
        // keep alive the supervisor that owns that same future.
        let dynamic_agent_spawner = dynamic_agent_spawner.task_handle();
        let request_constraints =
            spawn_child_request_constraints(&context.request_constraints, &config);
        let child_runtime_context = self
            .register_child_runtime_context(&context, &config, request_constraints.clone())
            .await;

        let mut profile =
            AgentProfile::new(&config.agent_id, &config.description, AgentTier::System);
        profile.system_prompt = Some(spawn_system_prompt(&config));
        profile.model_override = config.model.clone();
        profile.skill_filter = config.allowed_tools.clone();
        profile.metadata.insert(
            "spawn_agent_type".to_string(),
            json!(config.agent_type.clone()),
        );
        profile
            .metadata
            .insert("spawn_read_only".to_string(), json!(config.read_only));

        let mut subrun_context = HashMap::new();
        subrun_context.insert(
            "workspace_root".to_string(),
            json!(config.working_dir.to_string_lossy().to_string()),
        );
        subrun_context.insert(
            "cwd".to_string(),
            json!(config.working_dir.to_string_lossy().to_string()),
        );
        subrun_context.insert("spawn_agent_id".to_string(), json!(config.agent_id.clone()));
        subrun_context.insert(
            "spawn_agent_type".to_string(),
            json!(config.agent_type.clone()),
        );
        subrun_context.insert(
            "parent_run_id".to_string(),
            json!(context.parent_run_id.clone()),
        );
        subrun_context.insert(
            "parent_agent_id".to_string(),
            json!(
                config
                    .parent_address
                    .as_ref()
                    .map(|address| address.agent_id.clone())
                    .unwrap_or_else(|| "root-agent".to_string())
            ),
        );
        subrun_context.insert(
            "trace_session_id".to_string(),
            json!(context.trace_context.session_id.clone()),
        );
        subrun_context.insert(
            "trace_user_id".to_string(),
            json!(context.trace_context.user_id.clone()),
        );
        subrun_context.insert(
            "trace_turn_id".to_string(),
            json!(context.trace_context.turn_id.clone()),
        );
        subrun_context.insert(
            "trace_turn_seq".to_string(),
            json!(context.trace_context.turn_seq),
        );
        subrun_context.insert(
            "trace_causal_chain_id".to_string(),
            json!(context.trace_context.causal_chain_id.clone()),
        );
        subrun_context.insert(
            "trace_root_event_id".to_string(),
            json!(context.trace_context.root_event_id.clone()),
        );

        let mut child_permissions = config.inherited_permissions.clone();
        child_permissions.allowed_tools = request_constraints.allowed_tools.clone();
        let subrun = SubRunConfig {
            run_id: config.run_id.clone(),
            parent_run_id: context.parent_run_id.clone(),
            agent_profile: profile,
            task: config.task.clone(),
            session_id: context.session_id.clone(),
            user_id: context.user_id.clone(),
            previous_output: None,
            context: subrun_context,
            forward_headers: context.forward_headers.clone(),
            admitted_model_execution: context.admitted_model_execution.clone(),
            request_constraints,
            recursion_depth: config.recursion_depth,
            max_turns: Some(config.max_turns),
            pause_flag: child_runtime_context.pause_flag.clone(),
            checkpoint_gate: None,
            mailbox: config.mailbox,
            progress_emitter: config.progress_emitter.clone(),
            live_event_sink: config.live_event_sink.clone(),
            cancel_token: child_runtime_context.cancel_token.clone(),
            inherited_prefix: config.inherited_prefix,
            execution_metadata: config
                .execution_metadata
                .clone()
                .or_else(|| context.execution_metadata.clone()),
            delegation_chain: config.delegation_chain.clone(),
            #[cfg(feature = "harness")]
            harness_sink: context.harness_sink.clone(),
        };

        let executor = self.build_subrun_executor(
            child_permissions,
            dynamic_agent_spawner,
            config.client_tool_delivery_tx.clone(),
            context.admitted_model_execution.as_ref(),
        );
        #[cfg(feature = "bridge-e2e-hooks")]
        let executor = if !context.test_child_llm_rounds.is_empty() {
            executor.with_test_llm_rounds(context.test_child_llm_rounds.clone())
        } else {
            executor
        };
        let execution = AssertUnwindSafe(executor.execute(subrun))
            .catch_unwind()
            .await;
        self.remove_runtime_context(&config.run_id).await;
        let result = match execution {
            Ok(result) => result?,
            Err(_) => return Err("server dynamic child executor panicked".to_string()),
        };
        let projection = project_subrun_status_to_spawn(&result.status, result.error);

        Ok(SpawnRunResult {
            agent_id: result.agent_id,
            run_id: result.run_id,
            status: projection.status.to_string(),
            finish_reason: projection.finish_reason,
            cancelled_by_user: None,
            output: result.output,
            error: projection.error,
            prompt_tokens: result.prompt_tokens,
            completion_tokens: result.completion_tokens,
            tool_calls: result.tool_calls,
            // The shared delegation result does not currently expose loop
            // rounds. Keep this explicit instead of inventing a proxy from
            // tool or token counts.
            turns_completed: 0,
            permission_summary: None,
            permission_requests: 0,
            permission_requests_approved: 0,
            tools_blocked: 0,
        })
    }
}

fn child_uses_client_tool_delivery(
    parent_delivery_available: bool,
    bindings: Option<&ExecutionBindingSnapshot>,
) -> bool {
    parent_delivery_available
        && bindings.is_some_and(|snapshot| {
            matches!(snapshot.executor.transport, ToolTransportKind::EdgeLedger)
        })
}

/// Production sub-run executor backed by [`ServerAgenticLoopHost`].
///
/// Creates a real agentic loop for each sub-run with the agent's system prompt,
/// model, and tool configuration.
pub struct ServerSubRunExecutor {
    matrixone: MatrixOneSettings,
    encryptor: Arc<FernetTokenEncryptor>,
    /// Must be the lifecycle engine that created the parent run so owner leases,
    /// durable control, projection, and recovery share one authority.
    run_engine: Option<RunEngine>,
    shared_pool: Option<SharedPool>,
    /// Short-lived material inherited from an already admitted live parent.
    /// Recovery executors leave this empty and re-materialize by durable ID.
    admitted_model_execution: Option<astra_services::AdmittedModelExecution>,
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    edge_connection_pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    edge_dispatch_service: Option<Arc<dyn astra_services::multi_agent::EdgeDispatchService>>,
    edge_registry_service: Option<Arc<dyn astra_services::multi_agent::EdgeRegistryService>>,
    skill_service: Option<Arc<dyn SkillService>>,
    memory_extraction_service: Option<Arc<crate::session_memory::MemoryExtractionService>>,
    reflect_service: Arc<dyn astra_services::ReflectService>,
    inherited_permissions: InheritedPermissions,
    /// Present for dynamic `agent(action='spawn')` descendants.  Delegation
    /// engine sub-runs can omit it; dynamic children must receive the same
    /// session-owned spawner so they can create governed grandchildren.
    dynamic_agent_spawner: Option<Arc<DynamicAgentSpawner>>,
    /// Parent-owned delivery lane for browser/edge callback tool execution.
    client_tool_delivery_tx: Option<mpsc::Sender<Value>>,
    /// Shared ToolExecutionService so executors share the same disabled_tool_offers set.
    pub tool_execution_service: Option<ToolExecutionService>,
    #[cfg(feature = "bridge-e2e-hooks")]
    test_llm_rounds: Vec<Value>,
}

impl ServerSubRunExecutor {
    pub fn new(
        matrixone: MatrixOneSettings,
        encryptor: Arc<FernetTokenEncryptor>,
        edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    ) -> Self {
        Self {
            matrixone,
            encryptor,
            run_engine: None,
            shared_pool: None,
            admitted_model_execution: None,
            edge_callback_ledger,
            edge_connection_pool: None,
            edge_dispatch_service: None,
            edge_registry_service: None,
            skill_service: None,
            memory_extraction_service: None,
            reflect_service: Arc::new(astra_services::UnconfiguredReflectService),
            inherited_permissions: InheritedPermissions::auto_approve(),
            dynamic_agent_spawner: None,
            client_tool_delivery_tx: None,
            tool_execution_service: None,
            #[cfg(feature = "bridge-e2e-hooks")]
            test_llm_rounds: Vec::new(),
        }
    }

    pub fn with_run_engine(mut self, run_engine: RunEngine) -> Self {
        self.run_engine = Some(run_engine);
        self
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.shared_pool = Some(pool);
        self
    }

    pub fn with_admitted_model_execution(
        mut self,
        execution: Option<astra_services::AdmittedModelExecution>,
    ) -> Self {
        self.admitted_model_execution = execution;
        self
    }

    pub fn with_memory_extraction_service(
        mut self,
        svc: Arc<crate::session_memory::MemoryExtractionService>,
    ) -> Self {
        self.memory_extraction_service = Some(svc);
        self
    }

    pub fn with_inherited_permissions(
        mut self,
        inherited_permissions: InheritedPermissions,
    ) -> Self {
        self.inherited_permissions = inherited_permissions;
        self
    }

    fn with_dynamic_agent_spawner(mut self, spawner: Arc<DynamicAgentSpawner>) -> Self {
        self.dynamic_agent_spawner = Some(spawner);
        self
    }

    fn with_client_tool_delivery_tx(mut self, tx: Option<mpsc::Sender<Value>>) -> Self {
        self.client_tool_delivery_tx = tx;
        self
    }

    pub fn with_reflect_service(
        mut self,
        service: Arc<dyn astra_services::ReflectService>,
    ) -> Self {
        self.reflect_service = service;
        self
    }

    pub fn with_edge_connection_pool(
        mut self,
        pool: astra_server_types::edge_connection_pool::EdgeConnectionPool,
    ) -> Self {
        self.edge_connection_pool = Some(pool);
        self
    }

    pub fn with_edge_dispatch_service(
        mut self,
        svc: Arc<dyn astra_services::multi_agent::EdgeDispatchService>,
    ) -> Self {
        self.edge_dispatch_service = Some(svc);
        self
    }

    pub fn with_edge_registry_service(
        mut self,
        svc: Arc<dyn astra_services::multi_agent::EdgeRegistryService>,
    ) -> Self {
        self.edge_registry_service = Some(svc);
        self
    }

    pub fn with_skill_service(mut self, service: Arc<dyn SkillService>) -> Self {
        self.skill_service = Some(service);
        self
    }

    #[cfg(feature = "bridge-e2e-hooks")]
    pub fn with_test_llm_rounds(mut self, rounds: Vec<Value>) -> Self {
        self.test_llm_rounds = rounds;
        self
    }
}

impl ServerSubRunExecutor {
    fn durable_run_engine(&self) -> Option<RunEngine> {
        self.run_engine.clone()
    }

    async fn ensure_durable_subrun_started(&self, config: &SubRunConfig) -> Result<(), String> {
        let Some(run_engine) = self.durable_run_engine() else {
            return Ok(());
        };
        if run_engine
            .load_run(&config.user_id, &config.run_id)
            .await?
            .is_some()
        {
            return Ok(());
        }
        run_engine
            .start_run_ext_with_context(
                &config.run_id,
                &config.user_id,
                &config.session_id,
                Some(config.parent_run_id.as_str()),
                config.context.get("delegation_id").and_then(Value::as_str),
                Some(config.agent_profile.agent_id.as_str()),
                None,
                crate::server::run::engine::RunStartContext {
                    agent_binding_name: Some(config.agent_profile.name.clone()),
                    model_selection: config.admitted_model_execution.as_ref().map(|execution| {
                        ModelSelectionRequest {
                            offering_id: execution.offering_id.clone(),
                        }
                    }),
                    resolved_model_selection: config.admitted_model_execution.as_ref().map(
                        |execution| ResolvedModelSelection {
                            offering_id: execution.offering_id.clone(),
                            model_name: execution.model_name.clone(),
                        },
                    ),
                    ..Default::default()
                },
            )
            .await
    }

    async fn materialize_durable_subrun_execution(
        &self,
        config: &SubRunConfig,
    ) -> Result<Option<astra_services::AdmittedModelExecution>, String> {
        let inherited_execution = config
            .admitted_model_execution
            .as_ref()
            .or(self.admitted_model_execution.as_ref());
        let Some(run_engine) = self.durable_run_engine() else {
            return Ok(inherited_execution.cloned());
        };
        let run = run_engine
            .load_run(&config.user_id, &config.run_id)
            .await?
            .ok_or_else(|| {
                "durable sub-run disappeared before model materialization".to_string()
            })?;
        let offering_id = run.model_offering_id.as_deref().ok_or_else(|| {
            "durable sub-run is missing its admitted Offering identity".to_string()
        })?;
        let expected_model_name = run
            .resolved_model_name
            .as_deref()
            .ok_or_else(|| "durable sub-run is missing its resolved model identity".to_string())?;
        if let Some(execution) = inherited_execution {
            if execution.offering_id != offering_id || execution.model_name != expected_model_name {
                return Err(
                    "inherited model material does not match the durable sub-run Offering identity"
                        .to_string(),
                );
            }
            return Ok(Some(execution.clone()));
        }
        let offering = astra_services::resolve_active_llm_offering(
            &self.matrixone,
            self.encryptor.as_ref(),
            offering_id,
            self.shared_pool.as_ref().map(SharedPool::get),
        )
        .await
        .map_err(|error| error.to_string())?;
        if offering.model.model_name != expected_model_name {
            return Err(
                "durable sub-run Offering changed after admission; refusing route drift"
                    .to_string(),
            );
        }
        astra_services::AdmittedModelExecution::from_offering(offering).map(Some)
    }

    async fn persist_durable_subrun_status(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_code: Option<&str>,
        error_message: Option<&str>,
    ) {
        let Some(run_engine) = self.durable_run_engine() else {
            return;
        };
        let persisted = if let Some(error_code) = error_code {
            run_engine
                .transition_status_with_event_if_current(
                    user_id,
                    run_id,
                    &[STATUS_RUNNING],
                    status,
                    waiting_for,
                    error_message,
                    json!({
                        "event_type": "run_finished",
                        "data": {
                            "status": status,
                            "error_code": error_code,
                            "error": error_message,
                        }
                    }),
                )
                .await
        } else {
            run_engine
                .persist_status_if_current(
                    user_id,
                    run_id,
                    &[STATUS_RUNNING],
                    status,
                    waiting_for,
                    error_message,
                )
                .await
        };
        if let Err(error) = persisted {
            tracing::warn!(
                target: "astra_runtime::subrun",
                user_id,
                session_id,
                run_id,
                status,
                error = %error,
                "failed to persist durable subrun status"
            );
        }
    }

    async fn persist_durable_subrun_usage(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls: u32,
    ) {
        let Some(run_engine) = self.durable_run_engine() else {
            return;
        };
        if let Err(error) = run_engine
            .persist_usage(
                user_id,
                run_id,
                prompt_tokens,
                completion_tokens,
                tool_calls,
            )
            .await
        {
            tracing::warn!(
                target: "astra_runtime::subrun",
                user_id,
                session_id,
                run_id,
                prompt_tokens,
                completion_tokens,
                tool_calls,
                error = %error,
                "failed to persist durable subrun usage"
            );
        }
    }

    /// Provision a workspace directory for a delegation sub-run.
    ///
    /// Sub-runs get a subdirectory under the parent session workspace to
    /// keep file operations isolated while sharing the same base.
    fn provision_subrun_workspace(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> Result<std::path::PathBuf, String> {
        validate_workspace_id(session_id)
            .map_err(|source| format!("invalid sub-run session_id: {source}"))?;
        validate_workspace_id(run_id)
            .map_err(|source| format!("invalid sub-run run_id: {source}"))?;

        let base = std::env::var("ASTRA_SERVER_WORKSPACES")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("astra-workspaces"));
        let workspace = base.join(session_id).join(run_id);
        std::fs::create_dir_all(&workspace)
            .map_err(|error| format!("failed to create run workspace directory: {error}"))?;
        Ok(workspace)
    }
}

fn resolve_subrun_agentic_turn_budget(
    task_profile: astra_turn_core::chat_turn_heuristics::TaskExecutionProfile,
    explicit_max_turns: Option<u32>,
) -> astra_turn_core::chat_turn_heuristics::AgenticTurnBudget {
    let runtime_ceiling = astra_core::RuntimeLimits::global().max_turns;
    let Some(explicit_max_turns) = explicit_max_turns.map(|turns| turns as usize) else {
        return astra_turn_core::chat_turn_heuristics::resolve_agentic_turn_budget(
            task_profile,
            runtime_ceiling,
            None,
        );
    };

    // A child budget is an execution boundary selected by the caller after
    // agent-type and complexity resolution. Treating small values as mere
    // hints silently inflated focused review children from 12 turns to the
    // generic 60/90-turn profile budget. Preserve one source of truth all the
    // way into the child loop; the shared resolver still clamps an excessive
    // value to the process-wide runtime ceiling.
    astra_turn_core::chat_turn_heuristics::resolve_agentic_turn_budget(
        task_profile,
        runtime_ceiling,
        Some(
            astra_turn_core::chat_turn_heuristics::AgenticTurnBudgetOverride {
                initial_turns: Some(explicit_max_turns),
                hard_turn_limit: Some(explicit_max_turns),
            },
        ),
    )
}

#[async_trait]
impl SubRunExecutor for ServerSubRunExecutor {
    async fn execute(
        &self,
        config: SubRunConfig,
    ) -> Result<astra_services::coordination::AgentResult, String> {
        use astra_pipeline::step_protocol::InMemoryIdempotencyCache;
        use astra_text_utils::semantic_dedup::SemanticDedup;
        use astra_turn_core::chat_turn_heuristics::infer_task_execution_profile;
        use astra_turn_core::stop_hooks_yaml::{
            detect_turn_hook_sets, is_plan_subtask_from_delegation_context,
            project_root_from_delegation_context,
        };
        use astra_turn_core::turn_guard::TurnGuard;

        self.ensure_durable_subrun_started(&config).await?;
        let admitted_model_execution = self.materialize_durable_subrun_execution(&config).await?;
        if let Some(sink) = config.live_event_sink.as_ref()
            && let Err(error) = sink.send(AgentLiveEvent {
                run_id: config.run_id.clone(),
                agent_id: config.agent_profile.agent_id.clone(),
                kind: AgentLiveEventKind::Signal(
                    astra_turn_core::agent_live_event::AgentLiveSignal::RunStarted {
                        parent_run_id: Some(config.parent_run_id.clone()),
                        depth: u32::from(config.recursion_depth).saturating_add(1),
                        spawn_tool_call_id: None,
                        transcript_location:
                            astra_turn_types::AgentTranscriptLocation::DurableServer,
                    },
                ),
            })
        {
            tracing::debug!(
                agent_id = %config.agent_profile.agent_id,
                run_id = %config.run_id,
                ?error,
                "server subrun start was not delivered to the live workbench"
            );
        }
        let durable_run_engine = self.durable_run_engine();
        // Sub-runs are independently controllable durable runs.  They may
        // inherit their parent's cancellation token, but they never reuse the
        // parent's pause flag or cancellation flag: a direct child control
        // must not mutate root execution state.
        let local_cancel_flag = Arc::new(AtomicBool::new(false));
        let local_pause_flag = config
            .pause_flag
            .clone()
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let local_cancel_token = config
            .cancel_token
            .clone()
            .unwrap_or_else(|| Arc::new(CancellationToken::new()));
        let durable_run_control = durable_run_engine
            .clone()
            .map(|engine| Arc::new(engine) as Arc<dyn RunControlProvider>);
        // Keep the child lease alive for the entire executable section.  This
        // makes direct pause/resume admission truthful even when the parent
        // lives in a different process-local run map.
        let mut owner_lease_heartbeat = durable_run_engine.as_ref().and_then(|engine| {
            engine.start_owner_lease_heartbeat(config.user_id.clone(), config.run_id.clone())
        });
        let durable_user_id = config.user_id.clone();
        let durable_session_id = config.session_id.clone();
        let durable_run_id = config.run_id.clone();
        let memory_extraction_service = self.memory_extraction_service.as_ref().and_then(|svc| {
            match svc.scoped_to_owner(&config.user_id) {
                Ok(scoped) => Some(scoped),
                Err(error) => {
                    tracing::error!(
                        user_id = %config.user_id,
                        session_id = %config.session_id,
                        error = %error,
                        "sub-run session-memory extraction disabled because owner binding failed"
                    );
                    None
                }
            }
        });

        // Build edge profile from agent's system prompt and metadata.
        let compact_strategy = astra_turn_core::microcompact::CompactStrategy::from_provider_hint(
            config.agent_profile.model_override.as_deref().unwrap_or(""),
        );
        let mut edge_profile = Map::new();
        if let Some(prompt) = &config.agent_profile.system_prompt {
            edge_profile.insert(
                "system_prompt_override".to_string(),
                Value::String(prompt.clone()),
            );
        }
        if let Some(model) = &config.agent_profile.model_override {
            edge_profile.insert("model".to_string(), Value::String(model.clone()));
        }
        edge_profile.insert(
            "agent_id".to_string(),
            Value::String(config.agent_profile.agent_id.clone()),
        );
        let subrun_workspace =
            self.provision_subrun_workspace(&config.session_id, &config.run_id)?;
        let execution_bindings =
            execution_bindings_from_metadata(config.execution_metadata.as_ref(), &subrun_workspace);
        // Only a true thin-client callback transport may borrow the parent's
        // SSE `/tools/result` lane. An `edge_ws` workspace has an executable
        // server-to-edge dispatch service and must use RuntimeToolExecutor;
        // routing it through the browser callback lane waits forever because
        // the browser is not the selected workspace executor.
        let client_tool_delivery_available = child_uses_client_tool_delivery(
            self.client_tool_delivery_tx.is_some(),
            execution_bindings.as_ref(),
        );

        // Build the host with agent-specific configuration.
        let mut builder = ServerAgenticLoopHostBuilder::new(
            self.matrixone.clone(),
            self.encryptor.clone(),
            config.user_id.clone(),
            config.session_id.clone(),
        )
        .with_model(config.agent_profile.model_override.clone())
        .with_admitted_model_execution(admitted_model_execution)
        .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
            self.shared_pool.is_some(),
            self.reflect_service.is_configured(),
        ))
        .with_edge_profile(edge_profile)
        .with_edge_callback_ledger(self.edge_callback_ledger.clone());

        if let Some(pool) = &self.shared_pool {
            builder = builder.with_pool(pool.clone());
        }
        if let Some(svc) = &self.edge_dispatch_service {
            builder = builder.with_edge_dispatch_service(Arc::clone(svc));
        }
        if let Some(snapshot) = execution_bindings.as_ref() {
            builder = builder.with_execution_binding_snapshot(snapshot.clone());
        }
        #[cfg(feature = "bridge-e2e-hooks")]
        if !self.test_llm_rounds.is_empty() {
            builder = builder.with_test_llm_rounds(self.test_llm_rounds.clone());
        }
        // NOTE on grandchild inheritance: delegated children don't get
        // a prefix_store wired here because this sub-run executor
        // doesn't own one. Grandchild captures would be valuable for
        // deeper delegation trees but require threading the store into
        // `ServerSubRunExecutor` separately — scope-cut from G2 v1.
        if let Some(ref shared_tes) = self.tool_execution_service {
            builder = builder
                .with_disabled_tool_offers(shared_tes.disabled_tool_offers_handle())
                .with_provider_capabilities(shared_tes.provider_capabilities_handle())
                .with_provider_allowed_tools(shared_tes.provider_allowed_tools_handle());
        }
        let mut host = builder.build();
        host.set_client_cancel(local_cancel_flag.clone(), local_cancel_token.clone());
        if let Some(sink) = config.live_event_sink.clone() {
            host.set_agent_live_event_sink(
                config.run_id.clone(),
                config.agent_profile.agent_id.clone(),
                sink,
            );
        }
        if client_tool_delivery_available {
            let run_engine = self.run_engine.clone().ok_or_else(|| {
                "thin-client child interaction delivery requires a durable run engine".to_string()
            })?;
            host.set_interaction_sink(Arc::new(DurableHostInteractionSink {
                run_engine,
                user_id: config.user_id.clone(),
                run_id: config.run_id.clone(),
                session_id: config.session_id.clone(),
                agent_id: Some(config.agent_profile.agent_id.clone()),
                event_tx: Some(
                    self.client_tool_delivery_tx
                        .clone()
                        .expect("availability checked above"),
                ),
            }));
            host.prefer_client_tool_delivery();
        }

        // Build the task prompt, incorporating previous output if pipeline.
        let full_task = if let Some(prev) = &config.previous_output {
            format!("{}\n\nPrevious agent output:\n{}", config.task, prev)
        } else {
            config.task.clone()
        };

        let user_message = json!({
            "role": "user",
            "content": full_task,
        });

        let task_profile = infer_task_execution_profile(&full_task);
        let agentic_turn_budget =
            resolve_subrun_agentic_turn_budget(task_profile, config.max_turns);
        let max_turns = agentic_turn_budget.initial_turns;
        let project_root_buf = project_root_from_delegation_context(&config.context);
        let hook_sets = project_root_buf
            .as_ref()
            .map(|root| {
                detect_turn_hook_sets(
                    root.as_path(),
                    task_profile,
                    is_plan_subtask_from_delegation_context(&config.context),
                )
            })
            .unwrap_or_default();
        let workspace_root_hint = project_root_buf.map(|p| p.to_string_lossy().into_owned());
        let (tool_event_hooks, session_event_hooks) = workspace_root_hint
            .as_ref()
            .map(|root| crate::skills::hooks::load_all_hooks(std::path::Path::new(root)))
            .unwrap_or_default();

        let (skill_registry, raw_skill_resolver) =
            build_server_skill_resolver(self.skill_service.clone(), &config.user_id);
        let skill_resolver =
            apply_normalized_skill_allowlist(raw_skill_resolver, &config.request_constraints)?;

        // Sub-agent / delegation path: model comes from the agent profile
        // override, not a request field.
        let resolved_tool_policy = astra_config::runtime_config::RuntimeConfig::load()
            .tool_selection
            .resolve_for_model(config.agent_profile.model_override.as_deref());
        let permission_context = PermissionSyncContext::shared(self.inherited_permissions.clone());

        let mut loop_state = AgenticLoopState {
            messages: vec![user_message],
            run_transcript_capture: None,
            volatile_pending: Vec::new(),
            recent_rounds: Vec::new(),
            tool_results: Vec::new(),
            current_session_id: Some(config.session_id.clone()),
            current_run_id: Some(config.run_id.clone()),
            inference_purpose: astra_turn_types::InferencePurpose::SubAgent,
            context_manifest_pool: self.shared_pool.clone(),
            context_manifest_user_id: Some(config.user_id.clone()),
            context_manifest_model_name: config.agent_profile.model_override.clone(),
            runtime_manifest: None,
            recursion_depth: config.recursion_depth,
            final_text: String::new(),
            final_text_streamed: false,
            final_output_ready_notified: false,
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            total_observation_tool_calls: 0,
            has_any_usage: false,
            last_finish_reason: None,
            max_turns,
            remaining_turns: max_turns,
            turn_budget_hint_emitted_90: false,
            turn_budget_hint_emitted_50: false,
            turn_budget_hint_emitted_20: false,
            agentic_turn_budget,
            budget_policy: None,
            current_round_index: 0,
            llm_rounds_completed: 0,
            last_request_message_count: None,
            turn_guard: TurnGuard::new(),
            restricted_tools: std::collections::HashSet::new(),
            boosted_tools: std::collections::HashSet::new(),
            widen_selection_pending: false,
            step_recorder: StepRecorder::new(&config.user_id, &config.session_id, &config.run_id),
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(0.75),
            call_counts: HashMap::new(),
            max_identical_tool_calls: resolved_tool_policy.max_identical_tool_calls,
            max_tools_per_turn: resolved_tool_policy.max_tools_per_turn,
            repeated_cache_hit_suppression: resolved_tool_policy.repeated_cache_hit_suppression,
            max_consecutive_empty_name: resolved_tool_policy.max_consecutive_empty_name,
            stall: Default::default(),
            telemetry: Default::default(),
            skills: SkillState {
                registry_for_activation: if config.request_constraints.allowed_skills.is_some() {
                    None
                } else {
                    skill_registry
                },
                resolver: skill_resolver,
                request_constraints: config.request_constraints.clone(),
                quality_tracker: crate::skills::quality::SkillQualityTracker::new(),
                improvement_tracker: astra_skills::improvement::ImprovementTracker::new(),
                tool_event_hooks,
                session_event_hooks,
                ..Default::default()
            },
            hooks: StopHookState {
                stop_hooks: hook_sets.stop_hooks,
                teammate_idle_hooks: hook_sets.teammate_idle_hooks,
                workspace_root_hint,
                forward_headers: config.forward_headers.clone(),
                admitted_model_execution: config.admitted_model_execution.clone(),
                ..Default::default()
            },
            cancellation: CancellationState {
                flag: Some(local_cancel_flag.clone()),
                pause_flag: Some(local_pause_flag.clone()),
                token: Some(local_cancel_token.clone()),
            },
            messaging: MessagingState {
                mailbox: config.mailbox,
                progress_emitter: config.progress_emitter.clone(),
                ..Default::default()
            },
            user_intents: Default::default(),
            error_recovery: Default::default(),
            run_control: durable_run_control.clone(),
            pipeline_session: Some(
                astra_turn_core::pipeline_session::PipelineSession::new_with_current_date(
                    astra_turn_core::pipeline_config::PipelineConfig::default(),
                    crate::turn::session_current_date::resolve_session_current_date(
                        &config.session_id,
                    ),
                ),
            ),
            message: full_task.clone(),
            user_intent: full_task,
            recent_tools: Vec::new(),
            has_prior_assistant_turn: false,
            turn_intent: None,
            task_profile,
            last_turn_policy: crate::turn::agentic_loop::host::TurnInteractionPolicy::default(),
            api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None)
                .expect("valid dummy URL"),
            api_token: String::new(),
            delegation_engine: None,
            delegations_this_turn: 0,
            delegation_chain: config.delegation_chain.clone(),
            self_agent_id: config.agent_profile.agent_id.clone(),
            project_context: None,
            checkpoint_gate: config.checkpoint_gate.clone(),
            last_llm_context_manifest_trace: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            compaction_effectiveness: Default::default(),
            pinned_tool_schema_tokens: 0,
            sticky_tool_schemas: Vec::new(),
            max_turn_input_tokens: astra_core::RuntimeLimits::global().max_turn_input_tokens,
            budget_wrapup_injected: false,
            budget_wrapup_ignored_rounds: 0,
            compact_tier_applied: astra_turn_core::compaction_types::CompactionTier::Normal,
            skill_produced_output: false,
            max_cumulative_tokens: 0,
            thinking: astra_turn_core::thinking_config::ThinkingConfig::Off,
            recent_file_reads: Vec::new(),
            permission_context: Some(permission_context),
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            tool_budget_override: None,
            recent_tactical_actions: Vec::new(),
            runtime_tool_executor: None,
            interruption: None,
            session_facts: Default::default(),
            memory_extraction_service,
            observation_journal: Default::default(),
            observation_store: None,
            session_memory_state: Default::default(),
            session_memory_llm_params: None,
            compact_strategy,
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
            session_turn: 0,
            bridge_turn_chain_id: None,
            bridge_user_query_event_id: None,
            turn_event_buffer: None,
            harness: {
                #[cfg(feature = "harness")]
                {
                    match config.harness_sink {
                        Some(ref sink) => {
                            crate::turn::harness_adapter::HarnessSlot::observe_only(sink.clone())
                        }
                        None => crate::turn::harness_adapter::HarnessSlot::empty(),
                    }
                }
                #[cfg(not(feature = "harness"))]
                {
                    crate::turn::harness_adapter::HarnessSlot::empty()
                }
            },
        };
        if let Some(trace_context) = trace_context_from_subrun_context(&config.context) {
            loop_state.session_turn = u32::try_from(trace_context.turn_seq).unwrap_or(0);
        }

        // ── Wire RuntimeToolExecutor for sub-run tool execution ──────────
        // Without this, the headless pipeline fallback cannot execute tools
        // server-side and sub-agents would get edge-protocol errors.
        {
            let memoria_base = Some(astra_core::MemoriaSettings::from_env().base_url);
            let agent_working_dir = subrun_workspace.clone();
            let task_store = astra_tools::task_mgmt_matrixone::select_task_store(
                self.shared_pool.as_ref().map(|p| p.get().clone()),
                config.user_id.clone(),
            )?;
            let mut executor = runtime_tool_executor::RuntimeToolExecutor::new(
                subrun_workspace,
                config.user_id.clone(),
                config.session_id.clone(),
                memoria_base,
                None,
            );
            executor = wire_reflect_service_into_executor(executor, &self.reflect_service)
                .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
                    self.shared_pool.is_some(),
                    self.reflect_service.is_configured(),
                ))
                .with_cancel_token(Some(local_cancel_token.clone()))
                .with_task_store(task_store);

            // A child is a first-class durable run. Its approval and ask-user
            // interactions use the same journal/callback contract as the
            // root, rather than degrading to a missing-gate error merely
            // because this executor was launched by `agent(action='spawn')`.
            if let Some(run_engine) = durable_run_engine.clone() {
                let interaction_runs = Arc::new(RwLock::new(HashMap::new()));
                executor.set_approval_gate(Arc::new(
                    DurableRunApprovalGate::new(
                        config.user_id.clone(),
                        config.session_id.clone(),
                        config.run_id.clone(),
                        Some(loop_state.session_turn),
                        run_engine.clone(),
                        interaction_runs.clone(),
                        None,
                        None,
                    )
                    .with_cancel_token(local_cancel_token.clone()),
                ));
                executor.set_ask_user_gate(Arc::new(
                    DurableRunUserPromptGate::new(
                        config.user_id.clone(),
                        config.session_id.clone(),
                        config.run_id.clone(),
                        Some(loop_state.session_turn),
                        run_engine,
                        interaction_runs,
                        None,
                        None,
                    )
                    .with_cancel_token(local_cancel_token.clone()),
                ));
            }

            if let Some(pool) = self.shared_pool.as_ref() {
                executor.set_context_manifest_pool(pool.clone());
                executor = executor.with_session_artifact_store(
                    astra_services::DatabaseSessionArtifactStore::new(self.matrixone.clone())
                        .with_pool(pool.clone()),
                );
            }
            executor.enable_durable_invocations();

            // Apply shared ToolExecutionService (with admin-controllable disabled_tool_offers)
            // or fall back to building one from deployment config.
            if let Some(ref shared_tes) = self.tool_execution_service {
                executor = executor.with_tool_execution_service(shared_tes.clone());
            } else {
                let mut builder = apply_deployment_tool_policy(
                    ToolExecutionService::builder(),
                    &load_deployment_tool_policy(),
                );
                if let Some(pool) = &self.edge_connection_pool {
                    builder = builder.edge_connection_pool(pool.clone());
                }
                if let Some(svc) = &self.edge_dispatch_service {
                    builder = builder.edge_dispatch_service(Arc::clone(svc));
                }
                if let Some(svc) = &self.edge_registry_service {
                    builder = builder.edge_registry_service(Arc::clone(svc));
                }
                executor = executor.with_tool_execution_service(builder.build());
            }
            if let Some(shared) = self.shared_pool.as_ref() {
                executor.set_plan_repository(std::sync::Arc::new(
                    astra_plan::CloudPlanRepository::new(shared.get().clone()),
                ));
            }
            executor.set_plan_resume_hint_handle(host.plan_resume_hint_handle());
            executor.set_plan_authoring_active_handle(host.plan_authoring_active_handle());
            if let Some(obs) = loop_state.telemetry.observability_session.clone() {
                executor.set_observability_session(obs);
            }
            if let Some(snapshot) = execution_bindings {
                executor.set_execution_binding_snapshot(snapshot);
            }
            if let Some(spawner) = self.dynamic_agent_spawner.clone() {
                // A dynamic child is a full agent, not a terminal worker.
                // Reinstall the same typed runtime capability that roots
                // receive so a child may spawn and govern descendants.  This
                // stays out of prompt messages and keeps the session-owned
                // spawner as the one lifecycle authority at every depth.
                executor.set_agent_tool_context(AgentToolContext {
                    run_id: config.run_id.clone(),
                    agent_id: config.agent_profile.agent_id.clone(),
                    delegation_chain: config.delegation_chain.clone(),
                    current_model: config.agent_profile.model_override.clone(),
                    recursion_depth: config.recursion_depth,
                    is_fork_child: config.inherited_prefix.is_some(),
                    working_dir: agent_working_dir,
                    spawner,
                    inherited_permissions: self.inherited_permissions.clone(),
                    active_skills: Vec::new(),
                    live_event_sink: config.live_event_sink.clone(),
                    client_tool_delivery_tx: self.client_tool_delivery_tx.clone(),
                    trace_context: trace_context_from_subrun_context(&config.context),
                    execution_metadata: config.execution_metadata.clone(),
                    transcript_location: AgentTranscriptLocation::DurableServer,
                });
            }
            wire_executor_into_state(executor, &mut loop_state);
        }

        configure_runtime_controllers(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &mut loop_state,
            &config.user_id,
            &config.session_id,
        )
        .await;

        let durable_control_watcher = start_active_run_control_watcher(
            durable_run_control,
            config.user_id.clone(),
            config.run_id.clone(),
            local_cancel_flag,
            local_pause_flag,
            local_cancel_token,
        );

        let live_started_at = Instant::now();
        let live_agent_id = config.agent_profile.agent_id.clone();
        let outcome = run_agentic_loop_with_host(&mut host, &mut loop_state).await;
        let outcome = host.settle_loop_outcome(outcome);
        if matches!(&outcome, Ok(AgenticLoopOutcome::Completed)) {
            crate::turn::agentic_loop::finalization::mark_execution_incomplete_from_turn_evaluation(
                &mut loop_state,
            );
        }

        // Commit the durable lifecycle fact before slower transcript,
        // observability, and cleanup work. Otherwise a completed child can
        // remain visibly running for seconds while its executor has already
        // stopped, and a control request will be admitted against stale
        // liveness.
        let prompt_tokens = loop_state.provider_input_tokens();
        let projected_status = server_subrun_outcome_status(&outcome, &loop_state);
        let durable_status = server_subrun_durable_status(&outcome, &loop_state);
        let interruption_reason = server_subrun_interruption_reason(&loop_state);
        let durable_error =
            server_subrun_durable_error(&outcome, projected_status, interruption_reason.as_deref());
        let durable_error_code = server_subrun_durable_error_code(projected_status);
        let waiting_for =
            server_subrun_waiting_for(&outcome, durable_status, interruption_reason.as_deref());
        self.persist_durable_subrun_status(
            &durable_user_id,
            &durable_session_id,
            &durable_run_id,
            durable_status,
            waiting_for,
            durable_error_code,
            durable_error.as_deref(),
        )
        .await;
        self.persist_durable_subrun_usage(
            &durable_user_id,
            &durable_session_id,
            &durable_run_id,
            prompt_tokens,
            loop_state.total_completion,
            loop_state.total_tool_calls,
        )
        .await;
        drop(durable_control_watcher);
        drop(owner_lease_heartbeat.take());

        // Fire SessionEnd hooks (best-effort).
        crate::skills::hooks::fire_session_end(
            &loop_state.skills.session_event_hooks,
            loop_state.current_session_id.as_deref().unwrap_or(""),
        )
        .await;
        persist_runtime_promotion_events(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &config.user_id,
            &config.session_id,
            &config.run_id,
            &loop_state.telemetry.promotion_events,
        )
        .await?;
        persist_turn_evaluation_journal(&config.session_id, "server_subrun", &loop_state);
        flush_turn_observability(&mut loop_state, &config.session_id, false);

        // Persist core events for delegation sub-runs.
        persist_server_loop_core_events(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &config.user_id,
            &config.session_id,
            &config.run_id,
            Some(config.parent_run_id.as_str()),
            Some(config.agent_profile.agent_id.as_str()),
            config
                .context
                .get("parent_agent_id")
                .and_then(Value::as_str),
            trace_context_from_subrun_context(&config.context),
            &config.task,
            &loop_state,
            config.agent_profile.model_override.as_deref(),
        )
        .await;
        persist_server_loop_trace_events(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &config.user_id,
            &config.session_id,
            &config.run_id,
            Some(config.parent_run_id.as_str()),
            Some(config.agent_profile.agent_id.as_str()),
            config
                .context
                .get("parent_agent_id")
                .and_then(Value::as_str),
            trace_context_from_subrun_context(&config.context),
            &loop_state,
            config.agent_profile.model_override.as_deref(),
        )
        .await;
        let committed_assistant = match persist_server_loop_transcript_items(
            self.shared_pool.as_ref(),
            &config.user_id,
            &config.session_id,
            &config.run_id,
            trace_context_from_subrun_context(&config.context).as_ref(),
            &config.task,
            &loop_state,
            true,
        )
        .await
        {
            Ok(source_event_id) => source_event_id,
            Err(error) => {
                tracing::warn!(
                    session_id = %config.session_id,
                    run_id = %config.run_id,
                    %error,
                    "subrun transcript was not committed; live suffix will be retained"
                );
                None
            }
        };
        if let Some(pool) = self.shared_pool.as_ref()
            && let Err(error) = materialize_server_run_transcript_evidence(
                pool,
                &config.user_id,
                &config.session_id,
                &config.run_id,
                None,
            )
            .await
        {
            tracing::warn!(
                session_id = %config.session_id,
                run_id = %config.run_id,
                %error,
                "subrun durable transcript evidence materialization failed"
            );
        }

        if let Some(source_event_id) = committed_assistant {
            emit_server_subrun_transcript_committed(
                config.live_event_sink.as_ref(),
                &config.run_id,
                &live_agent_id,
                source_event_id,
            );
        }
        match &outcome {
            Ok(AgenticLoopOutcome::Waiting(reason)) => emit_server_subrun_execution_waiting(
                config.live_event_sink.as_ref(),
                &config.run_id,
                &live_agent_id,
                reason.clone(),
            ),
            _ => {
                if let Some(termination) = server_subrun_live_termination(&outcome, &loop_state) {
                    emit_server_subrun_agent_terminated(
                        config.live_event_sink.as_ref(),
                        &config.run_id,
                        &live_agent_id,
                        live_started_at,
                        termination,
                        server_subrun_live_reason(&outcome, &loop_state),
                    );
                }
            }
        }
        match outcome {
            Ok(AgenticLoopOutcome::Delegated) => Ok(astra_services::coordination::AgentResult {
                agent_id: config.agent_profile.agent_id,
                run_id: config.run_id,
                status: STATUS_DELEGATED.to_string(),
                output: None,
                error: None,
                prompt_tokens,
                completion_tokens: loop_state.total_completion,
                tool_calls: loop_state.total_tool_calls,
            }),
            Ok(AgenticLoopOutcome::ControlRejected(rejection)) => {
                Ok(astra_services::coordination::AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: STATUS_FAILED.to_string(),
                    output: None,
                    error: Some(format!("{}: {}", rejection.code, rejection.message)),
                    prompt_tokens,
                    completion_tokens: loop_state.total_completion,
                    tool_calls: loop_state.total_tool_calls,
                })
            }
            Ok(AgenticLoopOutcome::Completed) => Ok(astra_services::coordination::AgentResult {
                agent_id: config.agent_profile.agent_id,
                run_id: config.run_id,
                status: projected_status.to_string(),
                output: if loop_state.final_text.is_empty() {
                    None
                } else {
                    Some(loop_state.final_text)
                },
                error: if projected_status == STATUS_COMPLETED {
                    None
                } else {
                    interruption_reason
                },
                prompt_tokens,
                completion_tokens: loop_state.total_completion,
                tool_calls: loop_state.total_tool_calls,
            }),
            Ok(AgenticLoopOutcome::Cancelled) => Ok(astra_services::coordination::AgentResult {
                agent_id: config.agent_profile.agent_id,
                run_id: config.run_id,
                status: projected_status.to_string(),
                output: if loop_state.final_text.is_empty() {
                    None
                } else {
                    Some(loop_state.final_text)
                },
                error: None,
                prompt_tokens,
                completion_tokens: loop_state.total_completion,
                tool_calls: loop_state.total_tool_calls,
            }),
            Ok(AgenticLoopOutcome::Waiting(reason)) => {
                Ok(astra_services::coordination::AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: STATUS_WAITING.to_string(),
                    output: Some(reason),
                    error: None,
                    prompt_tokens,
                    completion_tokens: loop_state.total_completion,
                    tool_calls: loop_state.total_tool_calls,
                })
            }
            Ok(AgenticLoopOutcome::Error(err)) => Ok(astra_services::coordination::AgentResult {
                agent_id: config.agent_profile.agent_id,
                run_id: config.run_id,
                status: projected_status.to_string(),
                output: None,
                error: Some(err),
                prompt_tokens,
                completion_tokens: loop_state.total_completion,
                tool_calls: loop_state.total_tool_calls,
            }),
            Err(err) => Ok(astra_services::coordination::AgentResult {
                agent_id: config.agent_profile.agent_id,
                run_id: config.run_id,
                status: projected_status.to_string(),
                output: None,
                error: Some(err.to_string()),
                prompt_tokens,
                completion_tokens: loop_state.total_completion,
                tool_calls: loop_state.total_tool_calls,
            }),
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
