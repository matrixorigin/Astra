//! Shared handler for the consolidated `agent` tool.
//!
//! CLI and server execution environments own different child-loop
//! executors, but the `agent(action='spawn'|'get_result')` contract is
//! runtime semantics. Keep parsing, normalization, mailbox routing, lifecycle
//! dispatch, and result rendering here so Web/server cannot drift from CLI
//! behavior.

use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::future::join_all;

use astra_core::work_unit::{
    WORK_UNIT_OBSERVATION_FIELD, WorkUnitObservation, WorkUnitObservationMode, WorkUnitStatus,
    WorkUnitWakePolicy,
};
use astra_tools::agent_tool_contract::{
    AgentAction, AgentFanoutAction, agent_action_from_args, agent_fanout_action_from_args,
    has_malformed_tool_args,
};
use astra_turn_core::orchestration::agent_result_wire::{
    agent_tool_result_needs_recovery, fanout_slot_status_is_recoverable_issue,
    render_agent_tool_error, render_agent_tool_error_with_kind, render_unknown_agent_result,
    render_wait_for_agent_status, render_wait_timeout_outcome,
};
use astra_turn_core::orchestration_fanout_group::{
    AgentFanoutGroupProjection, AgentFanoutSlotStatus,
};

use super::{
    DynamicAgentSpawner, InheritedPermissions, SpawnAgentInput, SpawnAgentOutput, SpawnContext,
    SpawnError, WaitForAgentOutcome,
};
use astra_messaging::{
    AgentAddress, AgentMessage, MessagePayload, MessageTarget, types::RequestType,
};
use astra_turn_core::trace_event::TraceContext;

/// Maximum byte length we accept for an `agent_id` argument before
/// rejecting the request without echoing the value. Bytes (not chars)
/// because the limit is really about prompt-injection / log-bloat budget.
const MAX_AGENT_ID_BYTES: usize = 256;
/// `get_result` observes work that the user explicitly moved to the
/// background. Foreground spawn/fanout already return their terminal result
/// through the owning tool call, so this short grace period only closes races
/// with detached children that are already finishing.
const AGENT_RESULT_OBSERVE_GRACE: Duration = Duration::from_secs(1);
/// Total aggregate byte limit for the combined `results[]` array in
/// `get_results`/start-that-completed. If exceeded, per-slot limits
/// are proportionally reduced until the total fits.
const MAX_FANOUT_AGGREGATE_BYTES: usize = 60_000;
const FANOUT_RESULT_DEFAULT_MAX_BYTES: usize = 8_192;
const FANOUT_RESULT_MAX_BYTES: usize = 65_536;
static NEXT_FANOUT_GROUP_ID: AtomicU64 = AtomicU64::new(1);
/// Static prose for the `Unknown` outcome. Must NOT interpolate the
/// caller-supplied agent_id — that value already appears in the
/// structured `agent_id` JSON field, where serde escapes it safely.
const UNKNOWN_AGENT_ID_ERROR: &str = "Unknown agent_id. Use the exact runtime-generated agent_id returned by the earlier spawn result. The optional spawn `name` is only for send_message addressing and cannot be used with get_result.";

/// Authoritative storage for the child run's canonical transcript.
///
/// This is deliberately separate from control ownership: a client may read a
/// durable transcript before it has received the server's current
/// pause/resume/cancel capabilities. Conflating the two made a launch receipt
/// invent a local control target for server-owned fanout children.
pub use astra_turn_types::AgentTranscriptLocation;

fn render_spawn_agent_output(
    output: SpawnAgentOutput,
    transcript_location: AgentTranscriptLocation,
) -> String {
    let mut value = match serde_json::to_value(&output) {
        Ok(value) => value,
        Err(_) => return render_agent_tool_error(None, "Failed to serialize output"),
    };
    let Some(object) = value.as_object_mut() else {
        return render_agent_tool_error(None, "Failed to serialize output");
    };
    object.insert(
        "transcript_location".to_string(),
        Value::String(transcript_location.wire_value().to_string()),
    );
    if object.get("status").and_then(Value::as_str) == Some("launched") {
        object.insert(
            "lifecycle".to_string(),
            Value::String("running".to_string()),
        );
        object.insert(
            "delivery".to_string(),
            Value::String("explicit_background_handoff".to_string()),
        );
        object.insert(
            "instruction".to_string(),
            Value::String(
                "The user moved this child to the background. Its terminal result remains attached to this session and will be delivered to the parent mailbox. Do not claim completion before that result arrives; use send_message for corrections and get_result only for explicit inspection."
                    .to_string(),
            ),
        );
    }
    if object.get("status").and_then(Value::as_str) == Some("failed")
        && object.get("finish_reason").and_then(Value::as_str) == Some("executor_dropped")
    {
        object.insert(
            "diagnostic".to_string(),
            Value::String("executor_dropped".to_string()),
        );
        object.insert(
            "instruction".to_string(),
            Value::String(
                "The child run was scheduled but its foreground completion payload was lost. \
                 Do not retry the agent spawn or create replacement sub-agents — the run already \
                 executed and a duplicate would double the side effects. \
                 If the child was read-only and its partial progress is recoverable, you may call \
                 `get_result` once with the agent_id above to retrieve whatever was observed. \
                 Otherwise continue with currently bound local tools and report that the \
                 multi-agent runtime lost the child completion."
                    .to_string(),
            ),
        );
        object.insert("retryable".to_string(), Value::Bool(false));
    }
    serde_json::to_string(&value)
        .unwrap_or_else(|_| render_agent_tool_error(None, "Failed to serialize output"))
}

pub fn render_agent_runtime_binding_error(tool_name: &str, action: &str) -> String {
    let error = astra_turn_core::tool::runtime_binding::runtime_binding_denial_message(
        tool_name,
        Some(action),
    );
    render_agent_tool_error_with_kind(None, &error, Some(astra_core::ErrorKind::ToolBinding))
}

fn render_agent_tool_contract_error(message: &str) -> String {
    render_agent_tool_error_with_kind(None, message, Some(astra_core::ErrorKind::ToolInvalidArgs))
}

#[derive(Default)]
struct FanoutStartTerminalCauses {
    cancelled_by_user: usize,
    cancelled_by_parent_budget: usize,
    timed_out: usize,
    executor_dropped: usize,
    interrupted: usize,
    failed: usize,
}

impl FanoutStartTerminalCauses {
    fn from_agents(agents: &[Value]) -> Self {
        let mut causes = Self::default();
        for agent in agents {
            let status = agent.get("status").and_then(Value::as_str);
            let finish_reason = agent.get("finish_reason").and_then(Value::as_str);
            match status {
                Some("cancelled") => {
                    if agent
                        .get("cancelled_by_user")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        causes.cancelled_by_user += 1;
                    } else if finish_reason.is_some_and(is_parent_budget_fanout_finish_reason) {
                        causes.cancelled_by_parent_budget += 1;
                    } else {
                        causes.interrupted += 1;
                    }
                }
                Some("interrupted") => match finish_reason {
                    Some(reason) if is_parent_budget_fanout_finish_reason(reason) => {
                        causes.cancelled_by_parent_budget += 1;
                    }
                    Some(reason) if is_timeout_fanout_finish_reason(reason) => {
                        causes.timed_out += 1;
                    }
                    _ => causes.interrupted += 1,
                },
                Some("failed") => match finish_reason {
                    Some("executor_dropped") => causes.executor_dropped += 1,
                    Some(reason) if is_timeout_fanout_finish_reason(reason) => {
                        causes.timed_out += 1;
                    }
                    _ => causes.failed += 1,
                },
                _ => {}
            }
        }
        causes
    }

    fn has_stopped_slots(&self) -> bool {
        self.cancelled_by_user
            + self.cancelled_by_parent_budget
            + self.timed_out
            + self.executor_dropped
            + self.interrupted
            + self.failed
            > 0
    }

    fn insert_json_fields(&self, object: &mut serde_json::Map<String, Value>) {
        let mut causes = Vec::new();
        if self.cancelled_by_user > 0 {
            object.insert("cancelled_by_user".into(), json!(self.cancelled_by_user));
            causes.push("user_cancelled");
        }
        if self.cancelled_by_parent_budget > 0 {
            object.insert(
                "cancelled_by_parent_budget".into(),
                json!(self.cancelled_by_parent_budget),
            );
            causes.push("parent_budget");
        }
        if self.timed_out > 0 {
            object.insert("timed_out".into(), json!(self.timed_out));
            causes.push("timeout");
        }
        if self.executor_dropped > 0 {
            object.insert("executor_dropped".into(), json!(self.executor_dropped));
            causes.push("executor_dropped");
        }
        if self.interrupted > 0 {
            object.insert("interrupted".into(), json!(self.interrupted));
            causes.push("interrupted");
        }
        if self.failed > 0 {
            object.insert("failed".into(), json!(self.failed));
            causes.push("failed");
        }
        if !causes.is_empty() {
            object.insert("interruption_causes".into(), json!(causes));
        }
    }
}

fn is_parent_budget_fanout_finish_reason(reason: &str) -> bool {
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

fn is_timeout_fanout_finish_reason(reason: &str) -> bool {
    matches!(reason, "timeout" | "timed_out" | "deadline_exceeded")
}

/// Context for executing `agent` tool lifecycle actions.
#[derive(Clone)]
pub struct AgentToolContext {
    /// Current agent's run ID.
    pub run_id: String,
    /// Current agent's ID.
    pub agent_id: String,
    /// Chain of agent_ids that led to this agent (for circular delegation detection).
    /// Inherited from parent delegation and appended with parent agent_id.
    /// Format: ["orchestrator", "coder", "reviewer"] means orchestrator→coder→reviewer.
    pub delegation_chain: Vec<String>,
    /// Current active model for the parent turn. Used as the default
    /// child model when the tool call omits an explicit override.
    pub current_model: Option<String>,
    /// Current nested agent/sub-run depth of the agent.
    pub recursion_depth: u8,
    /// Whether this agent already inherited a fork prefix.
    pub is_fork_child: bool,
    /// Working directory inherited by the child unless isolation changes it.
    pub working_dir: PathBuf,
    /// Shared lifecycle owner for dynamic child agents.
    pub spawner: Arc<DynamicAgentSpawner>,
    /// Effective permissions inherited by children spawned from this agent.
    pub inherited_permissions: InheritedPermissions,
    /// Skills available to this agent and inherited by children.
    pub active_skills: Vec<String>,
    /// Optional sink for live child token/tool/status events.
    pub live_event_sink: Option<astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
    /// Per-run client lane for executable edge tool requests from spawned
    /// children. This must never be stored in a session-owned registry.
    pub client_tool_delivery_tx: Option<tokio::sync::mpsc::Sender<Value>>,
    /// DB trace identity shared with the current Web turn.
    pub trace_context: Option<TraceContext>,
    /// UI/runtime execution binding metadata inherited by child agents.
    pub execution_metadata: Option<Value>,
    /// Where the canonical transcript for dynamic child runs is persisted.
    /// This is emitted in every spawn/fanout receipt so the client can open
    /// the right history without guessing from a control endpoint.
    pub transcript_location: AgentTranscriptLocation,
}

/// Handle the consolidated `agent` tool for shared dynamic-agent actions.
///
/// Environment-specific actions such as `run_chain` can still be handled by
/// the caller before/after this function. This shared handler intentionally
/// owns spawn/get_result/send_message validation and rendering.
pub async fn handle_agent_tool(args: &Value, ctx: Option<&AgentToolContext>) -> String {
    if has_malformed_tool_args(args) {
        return astra_turn_core::orchestration::agent_result_wire::render_agent_tool_malformed_arguments_error("agent");
    }
    let action = match agent_action_from_args(args) {
        Ok(action) => action,
        Err(error) => return render_agent_tool_contract_error(&error),
    };
    match action {
        AgentAction::Spawn => handle_agent_spawn_action(args, ctx).await,
        AgentAction::GetResult => handle_agent_get_result_action(args, ctx).await,
        AgentAction::SendMessage => handle_agent_send_message_action(args, ctx).await,
        AgentAction::RunChain => render_agent_tool_error(
            None,
            "agent.run_chain is owned by the executor chain engine and cannot be handled by the shared agent lifecycle handler.",
        ),
    }
}

fn rejected_agent_message(reason: impl Into<String>) -> String {
    json!({
        "success": false,
        "status": "rejected",
        "reason": reason.into(),
    })
    .to_string()
}

fn agent_message_content(args: &Value) -> Result<String, String> {
    let message = args
        .get("message")
        .ok_or_else(|| "send_message requires `message`".to_string())?;
    let content = match message {
        Value::String(content) => content.clone(),
        other => serde_json::to_string(other)
            .map_err(|_| "send_message could not serialize `message`".to_string())?,
    };
    let content = content.trim();
    if content.is_empty() {
        return Err("send_message requires a non-empty `message`".to_string());
    }
    if content.chars().count() > 20_000 {
        return Err("send_message `message` exceeds 20000 characters".to_string());
    }
    Ok(content.to_string())
}

fn agent_message_payload(message_type: &str, content: &str) -> Result<MessagePayload, String> {
    match message_type {
        "text" | "answer" | "instruction" | "shutdown_response" => Ok(MessagePayload::Text {
            content: content.to_string(),
            summary: Some(message_type.replace('_', " ")),
        }),
        "progress" => Ok(MessagePayload::Progress {
            turn_index: 0,
            tool_calls: 0,
            status: "in_progress".to_string(),
            detail: Some(content.to_string()),
        }),
        "question" => Ok(MessagePayload::Request {
            request_type: RequestType::Custom("question".to_string()),
            data: json!({"content": content}),
        }),
        // Runtime owns lifecycle truth and automatically emits the real
        // terminal Completed/Failed signal. A model-authored "result" is a
        // semantic report, not proof that the child run has terminated.
        "result" => Ok(MessagePayload::Text {
            content: content.to_string(),
            summary: Some("result report".to_string()),
        }),
        "shutdown_request" => Ok(MessagePayload::Request {
            request_type: RequestType::Shutdown,
            data: json!({"reason": content}),
        }),
        other => Err(format!(
            "unsupported message_type '{other}'; use text, question, answer, instruction, progress, result, shutdown_request, or shutdown_response"
        )),
    }
}

async fn resolve_agent_message_target(
    router: &astra_messaging::router::AgentMailboxRouter,
    run_id: &str,
    recipient: &str,
) -> Result<(MessageTarget, String, Option<Vec<String>>), String> {
    let recipient = recipient.trim();
    if recipient.is_empty() {
        return Err("send_message requires a non-empty `to`".to_string());
    }
    match recipient.to_ascii_lowercase().as_str() {
        "parent" | "orchestrator" => {
            if router.parent_run_id(run_id).await.is_none() {
                return Err("the root agent has no parent target".to_string());
            }
            return Ok((MessageTarget::Parent, "parent".to_string(), None));
        }
        "*" | "broadcast" | "all" | "peers" => {
            // Broadcast has one stable meaning: peers in the sender's current
            // delegation. A root's delegation is its children; a child uses
            // its parent's namespace. Do not switch meaning merely because a
            // nested agent happened to spawn children of its own.
            let namespace = router
                .parent_run_id(run_id)
                .await
                .unwrap_or_else(|| run_id.to_string());
            let recipients = router
                .list_registered_agents(&namespace)
                .await
                .map_err(|error| format!("broadcast target lookup failed: {error}"))?;
            let recipient_ids = recipients
                .iter()
                .filter(|address| address.run_id != run_id)
                .map(|address| address.agent_id.clone())
                .collect::<Vec<_>>();
            if recipient_ids.is_empty() {
                return Err("no active peer agents are available for broadcast".to_string());
            }
            return Ok((
                MessageTarget::Broadcast {
                    delegation_id: namespace,
                },
                "broadcast".to_string(),
                Some(recipient_ids),
            ));
        }
        _ => {}
    }

    if let Some(address) = router.registered_address(recipient).await {
        let sender_parent = router.parent_run_id(run_id).await;
        let target_parent = router.parent_run_id(&address.run_id).await;
        let related = target_parent.as_deref() == Some(run_id)
            || sender_parent.as_deref() == Some(address.run_id.as_str())
            || sender_parent.is_some() && sender_parent == target_parent;
        if !related {
            return Err(format!(
                "target run_id '{recipient}' is outside the sender's parent/child/peer delegation boundary"
            ));
        }
        let display = format!("{}@{}", address.agent_id, address.run_id);
        return Ok((MessageTarget::Direct { address }, display, None));
    }
    if let Ok(address) = router.resolve_agent(run_id, recipient).await {
        let display = format!("{}@{}", address.agent_id, address.run_id);
        return Ok((MessageTarget::Direct { address }, display, None));
    }
    if let Some(parent_run_id) = router.parent_run_id(run_id).await
        && let Ok(address) = router.resolve_agent(&parent_run_id, recipient).await
    {
        let display = format!("{}@{}", address.agent_id, address.run_id);
        return Ok((MessageTarget::Direct { address }, display, None));
    }
    Err(format!(
        "target '{recipient}' is not an active child, peer, or exact run_id"
    ))
}

async fn handle_agent_send_message_action(args: &Value, ctx: Option<&AgentToolContext>) -> String {
    let Some(ctx) = ctx else {
        return render_agent_runtime_binding_error("agent", "send_message");
    };
    let router = ctx.spawner.mailbox_router();
    handle_agent_send_message_with_router(args, router.as_ref(), &ctx.run_id, &ctx.agent_id).await
}

/// Canonical mailbox-backed `agent.send_message` implementation. Callers
/// that own a mailbox but do not expose a dynamic-agent spawn context (for
/// example skill sub-runs) use this entry point so routing and receipts do not
/// fork into a second protocol.
pub async fn handle_agent_send_message_with_router(
    args: &Value,
    router: &astra_messaging::router::AgentMailboxRouter,
    run_id: &str,
    agent_id: &str,
) -> String {
    let content = match agent_message_content(args) {
        Ok(content) => content,
        Err(error) => return rejected_agent_message(error),
    };
    let message_type = args
        .get("message_type")
        .and_then(Value::as_str)
        .unwrap_or("text");
    let payload = match agent_message_payload(message_type, &content) {
        Ok(payload) => payload,
        Err(error) => return rejected_agent_message(error),
    };
    let recipient = match args.get("to").and_then(Value::as_str) {
        Some(recipient) => recipient,
        None => return rejected_agent_message("send_message requires string field `to`"),
    };
    let (target, target_display, recipients) =
        match resolve_agent_message_target(router, run_id, recipient).await {
            Ok(target) => target,
            Err(error) => return rejected_agent_message(error),
        };

    // Replies/acks must target a mailbox that actually survives long enough
    // to receive them. Interactive root execution uses a turn-scoped run_id,
    // while its mailbox is session-scoped; child/server agents normally use
    // the same identity for both.
    let from = router
        .registered_address_for_agent(agent_id)
        .await
        .unwrap_or_else(|| AgentAddress::new(run_id, agent_id));
    let mut message = AgentMessage::new(from, target, payload).with_ack_required();
    if let Some(request_id) = args
        .get("request_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|request_id| !request_id.is_empty())
    {
        message = message.with_correlation(request_id);
    }
    let message_id = message.id.clone();
    if let Err(error) = router.send(message).await {
        return rejected_agent_message(format!("delivery rejected: {error}"));
    }

    json!({
        "success": true,
        "status": "queued",
        "message_id": message_id,
        "target": target_display,
        "recipients": recipients,
        "message_type": message_type,
        "acknowledgement": "The target runtime will emit applied acknowledgement after injecting this guidance at a model boundary.",
    })
    .to_string()
}

/// Handle the atomic `agent_fanout` tool.
pub async fn handle_agent_fanout_tool(args: &Value, ctx: Option<&AgentToolContext>) -> String {
    if has_malformed_tool_args(args) {
        return astra_turn_core::orchestration::agent_result_wire::render_agent_tool_malformed_arguments_error("agent_fanout");
    }
    let action = match agent_fanout_action_from_args(args) {
        Ok(action) => action,
        Err(error) => return render_agent_tool_contract_error(&error),
    };
    match action {
        AgentFanoutAction::Start => handle_agent_fanout_start_action(args, ctx).await,
        AgentFanoutAction::GetResults => handle_agent_fanout_get_results_action(args, ctx).await,
        AgentFanoutAction::StopSlot => handle_agent_fanout_stop_slot_action(args, ctx).await,
    }
}

/// Recover a missing `agent_fanout` edge result from the fanout registry.
///
/// The recovery path is idempotent for `start`: if the start call already
/// created a group for this parent run, return that group's results instead
/// of replaying the start and duplicating child agents.
pub async fn recover_agent_fanout_tool_result(
    args: &Value,
    tool_call_id: Option<&str>,
    ctx: Option<&AgentToolContext>,
) -> String {
    let action = match agent_fanout_action_from_args(args) {
        Ok(action) => action,
        Err(error) => return render_agent_tool_contract_error(&error),
    };
    let Some(ctx) = ctx else {
        return render_agent_runtime_binding_error("agent_fanout", action.as_str());
    };
    if action == AgentFanoutAction::GetResults {
        let mut get_args = args.clone();
        if let Some(tool_call_id) = tool_call_id
            && let Some(object) = get_args.as_object_mut()
        {
            object.insert(
                "_tool_call_id".to_string(),
                Value::String(tool_call_id.to_string()),
            );
        }
        return handle_agent_fanout_get_results_action(&get_args, Some(ctx)).await;
    }
    if action == AgentFanoutAction::StopSlot {
        return render_agent_tool_error(
            None,
            "Cannot recover missing agent_fanout.stop_slot result because stop_slot has side effects. Recovery never replays control actions that can mutate child-agent state; call agent_fanout(action='get_results', group_id=...) to inspect the current group.",
        );
    }

    let tool_call_id = tool_call_id
        .or_else(|| args.get("_tool_call_id").and_then(Value::as_str))
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let requested_group_id = args
        .get("group_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let groups = ctx.spawner.list_fanout_groups().await;
    let parent_groups: Vec<_> = groups
        .iter()
        .filter(|group| group.parent_run_id.as_deref() == Some(ctx.run_id.as_str()))
        .collect();

    if let Some(group_id) = requested_group_id
        && let Some(group) = parent_groups
            .iter()
            .find(|group| group.group_id == group_id)
    {
        return render_agent_fanout_results(
            ctx,
            &group.group_id,
            tool_call_id.map(str::to_string),
            FanoutResultReadOptions::default(),
        )
        .await;
    }
    if requested_group_id.is_some() {
        return render_agent_tool_error(
            None,
            &format!(
                "Cannot recover missing agent_fanout.start result: requested group_id '{}' does not exist for parent run '{}'. Recovery is read-only and will not start replacement agents.",
                requested_group_id.unwrap_or_default(),
                ctx.run_id
            ),
        );
    }
    if let Some(tool_call_id) = tool_call_id
        && let Some(group) = parent_groups
            .iter()
            .find(|group| group.created_by_tool_use_id.as_deref() == Some(tool_call_id))
    {
        return render_agent_fanout_results(
            ctx,
            &group.group_id,
            Some(tool_call_id.to_string()),
            FanoutResultReadOptions::default(),
        )
        .await;
    }
    if parent_groups.len() == 1 {
        let group = parent_groups[0];
        return render_agent_fanout_results(
            ctx,
            &group.group_id,
            tool_call_id.map(str::to_string),
            FanoutResultReadOptions::default(),
        )
        .await;
    }
    if parent_groups.len() > 1 {
        let group_ids: Vec<_> = parent_groups
            .iter()
            .map(|group| group.group_id.as_str())
            .collect();
        return render_agent_tool_error(
            None,
            &format!(
                "Cannot recover missing agent_fanout.start result unambiguously: parent run '{}' has multiple fanout groups: {}. Use agent_fanout(action='get_results', group_id=...) with one of those exact group_id values.",
                ctx.run_id,
                group_ids.join(", ")
            ),
        );
    }

    render_agent_tool_error(
        None,
        &format!(
            "Cannot recover missing agent_fanout.start result: parent run '{}' has no registered fanout group. Recovery is read-only and will not replay start or spawn replacement agents.",
            ctx.run_id
        ),
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentFanoutStartInput {
    #[serde(default, rename = "action")]
    _action: Option<String>,
    #[serde(default, rename = "_tool_call_id")]
    _tool_call_id: Option<String>,
    #[serde(default)]
    group_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    target_count: usize,
    slots: Vec<AgentFanoutStartSlot>,
    #[serde(default)]
    defaults: Option<AgentFanoutDefaults>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentFanoutStartSlot {
    #[serde(default, rename = "id")]
    slot_id: Option<String>,
    description: String,
    prompt: String,
    #[serde(default)]
    agent_type: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    max_turns: Option<u32>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    #[serde(default)]
    complexity: Option<String>,
    #[serde(default)]
    isolated: Option<bool>,
    #[serde(default)]
    allowed_tools: Option<Vec<String>>,
}

/// Shared runtime configuration defaults for all slots in a fanout group.
///
/// Any field set here is inherited by every slot unless the slot provides
/// its own override.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentFanoutDefaults {
    #[serde(default)]
    agent_type: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    max_turns: Option<u32>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    #[serde(default)]
    complexity: Option<String>,
    #[serde(default)]
    isolated: Option<bool>,
    #[serde(default)]
    allowed_tools: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentFanoutGroupInput {
    #[serde(default, rename = "action")]
    _action: Option<String>,
    #[serde(default, rename = "_tool_call_id")]
    _tool_call_id: Option<String>,
    group_id: String,
    #[serde(default)]
    slot_index: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentFanoutStopSlotInput {
    #[serde(default, rename = "action")]
    _action: Option<String>,
    #[serde(default, rename = "_tool_call_id")]
    _tool_call_id: Option<String>,
    group_id: String,
    slot_index: usize,
}

const FANOUT_START_FIELDS: &[&str] = &[
    "action",
    "_tool_call_id",
    "group_id",
    "title",
    "target_count",
    "slots",
    "defaults",
];
const FANOUT_DEFAULTS_FIELDS: &[&str] = &[
    "agent_type",
    "model",
    "max_turns",
    "max_output_tokens",
    "complexity",
    "isolated",
    "allowed_tools",
];
const FANOUT_SLOT_FIELDS: &[&str] = &[
    "id",
    "description",
    "prompt",
    "agent_type",
    "model",
    "max_turns",
    "max_output_tokens",
    "complexity",
    "isolated",
    "allowed_tools",
];
const FANOUT_GET_RESULTS_FIELDS: &[&str] = &[
    "action",
    "_tool_call_id",
    "group_id",
    "slot_index",
    "offset",
    "max_bytes",
];
const FANOUT_STOP_SLOT_FIELDS: &[&str] = &["action", "_tool_call_id", "group_id", "slot_index"];
const FANOUT_START_SHAPE: &str = "Use one JSON object: {\"action\":\"start\",\"target_count\":2,\"slots\":[{\"id\":\"api\",\"description\":\"Short UI label\",\"prompt\":\"Full child task prompt\"},{\"id\":\"review\",\"description\":\"Short UI label\",\"prompt\":\"Full child task prompt\"}],\"defaults\":{\"agent_type\":\"code-review\"}}. Put work instructions in each slots[i].prompt; there is no top-level brief or agents payload. Runtime config belongs in `defaults`, not at top level. Fanout waits for accepted children by default; only an explicit user Ctrl+B action moves the live group to the background. Do not pass run_in_background.";
const FANOUT_GET_RESULTS_SHAPE: &str = "Use one JSON object: {\"action\":\"get_results\",\"group_id\":\"returned-group-id\"}. For large results, use {\"action\":\"get_results\",\"group_id\":\"returned-group-id\",\"slot_index\":0,\"offset\":0,\"max_bytes\":8192}.";
const FANOUT_STOP_SLOT_SHAPE: &str = "Use one JSON object: {\"action\":\"stop_slot\",\"group_id\":\"returned-group-id\",\"slot_index\":0}.";

fn reject_unknown_fields_for_shape(
    object: &serde_json::Map<String, Value>,
    allowed_fields: &[&str],
    scope: &str,
    shape: &str,
) -> Result<(), String> {
    if let Some(field) = object
        .keys()
        .find(|field| !allowed_fields.contains(&field.as_str()))
    {
        return Err(format!(
            "unknown field `{field}` for {scope}. Valid fields: {}. {shape}",
            allowed_fields.join(", ")
        ));
    }
    Ok(())
}

fn validate_required_field(
    object: &serde_json::Map<String, Value>,
    field: &str,
    scope: &str,
    shape: &str,
) -> Result<(), String> {
    if object.contains_key(field) {
        Ok(())
    } else {
        Err(format!(
            "missing required field `{field}` for {scope}. {shape}"
        ))
    }
}

fn fanout_args_object<'a>(
    args: &'a Value,
    scope: &str,
    shape: &str,
) -> Result<&'a serde_json::Map<String, Value>, String> {
    args.as_object()
        .ok_or_else(|| format!("{scope} input must be a JSON object. {shape}"))
}

fn validate_agent_fanout_start_shape(args: &Value) -> Result<(), String> {
    let object = fanout_args_object(args, "agent_fanout.start", FANOUT_START_SHAPE)?;
    reject_unknown_fields_for_shape(
        object,
        FANOUT_START_FIELDS,
        "agent_fanout.start",
        FANOUT_START_SHAPE,
    )?;
    validate_required_field(
        object,
        "target_count",
        "agent_fanout.start",
        FANOUT_START_SHAPE,
    )?;
    validate_required_field(object, "slots", "agent_fanout.start", FANOUT_START_SHAPE)?;

    // Validate defaults object if present
    if let Some(defaults) = object.get("defaults") {
        let defaults_object = defaults.as_object().ok_or_else(|| {
            format!(
                "field `defaults` for agent_fanout.start must be an object, got {}. {}",
                match defaults {
                    Value::String(_) => "string",
                    Value::Array(_) => "array",
                    Value::Null => "null",
                    Value::Bool(_) => "bool",
                    Value::Number(_) => "number",
                    Value::Object(_) => unreachable!(),
                },
                FANOUT_START_SHAPE
            )
        })?;
        reject_unknown_fields_for_shape(
            defaults_object,
            FANOUT_DEFAULTS_FIELDS,
            "agent_fanout.start.defaults",
            FANOUT_START_SHAPE,
        )?;
    }

    let slots_value = object.get("slots").unwrap();
    let slots = slots_value.as_array().ok_or_else(|| {
        let actual_type = match slots_value {
            Value::String(_) => "string",
            Value::Object(_) => "object",
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::Array(_) => unreachable!(),
        };
        format!(
            "field `slots` for agent_fanout.start must be an array of slot objects, got {actual_type}. {FANOUT_START_SHAPE}"
        )
    })?;
    for (slot_index, slot) in slots.iter().enumerate() {
        let scope = format!("agent_fanout.start slots[{slot_index}]");
        let slot_object = slot.as_object().ok_or_else(|| {
            format!(
                "{scope} must be a JSON object with description and prompt. {FANOUT_START_SHAPE}"
            )
        })?;
        reject_unknown_fields_for_shape(
            slot_object,
            FANOUT_SLOT_FIELDS,
            &scope,
            FANOUT_START_SHAPE,
        )?;
        validate_required_field(slot_object, "description", &scope, FANOUT_START_SHAPE)?;
        validate_required_field(slot_object, "prompt", &scope, FANOUT_START_SHAPE)?;
    }
    Ok(())
}

fn validate_agent_fanout_group_shape(
    args: &Value,
    action: &str,
    allowed_fields: &[&str],
    shape: &str,
) -> Result<(), String> {
    let scope = format!("agent_fanout.{action}");
    let object = fanout_args_object(args, &scope, shape)?;
    reject_unknown_fields_for_shape(object, allowed_fields, &scope, shape)?;
    validate_required_field(object, "group_id", &scope, shape)
}

fn coerce_fanout_start_input(args: &mut Value) {
    let Some(object) = args.as_object_mut() else {
        return;
    };

    // Coerce target_count from string "5" to integer 5.
    if let Some(tc) = object.get_mut("target_count") {
        if let Some(s) = tc.as_str() {
            if let Ok(n) = s.parse::<u64>() {
                *tc = Value::Number(n.into());
            }
        }
    }

    // Coerce slots from stringified JSON to array.
    if let Some(slots_value) = object.get_mut("slots") {
        if !slots_value.is_array() {
            if let Some(s) = slots_value.as_str() {
                if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                    if parsed.is_array() {
                        *slots_value = parsed;
                    }
                }
            }
        }
    }

    // Coerce integer fields inside slots that may arrive as strings.
    if let Some(slots) = object.get_mut("slots").and_then(Value::as_array_mut) {
        for slot in slots {
            if let Some(obj) = slot.as_object_mut() {
                for key in &["max_turns", "max_output_tokens"] {
                    if let Some(v) = obj.get_mut(*key) {
                        if let Some(s) = v.as_str() {
                            if let Ok(n) = s.parse::<u64>() {
                                *v = Value::Number(n.into());
                            }
                        }
                    }
                }
            }
        }
    }

    // Coerce integer fields inside defaults that may arrive as strings.
    if let Some(defaults) = object.get_mut("defaults").and_then(Value::as_object_mut) {
        for key in &["max_turns", "max_output_tokens"] {
            if let Some(v) = defaults.get_mut(*key) {
                if let Some(s) = v.as_str() {
                    if let Ok(n) = s.parse::<u64>() {
                        *v = Value::Number(n.into());
                    }
                }
            }
        }
    }
}

async fn handle_agent_fanout_start_action(args: &Value, ctx: Option<&AgentToolContext>) -> String {
    let mut args = args.clone();
    coerce_fanout_start_input(&mut args);
    if let Err(e) = validate_agent_fanout_start_shape(&args) {
        return render_agent_tool_error(None, &format!("Invalid input: {e}"));
    }
    let ctx = match ctx {
        Some(c) => c,
        None => {
            return render_agent_runtime_binding_error("agent_fanout", "start");
        }
    };
    if !ctx.spawner.has_executor() {
        return render_agent_runtime_binding_error("agent_fanout", "start");
    }
    let mut input: AgentFanoutStartInput = match serde_json::from_value(args.clone()) {
        Ok(input) => input,
        Err(e) => {
            return render_agent_tool_error(
                None,
                &format!("Invalid input for agent_fanout.start: {e}. {FANOUT_START_SHAPE}"),
            );
        }
    };
    if input.target_count == 0 {
        return render_agent_tool_error(None, "Invalid input: target_count must be >= 1");
    }
    if input.target_count > 50 {
        return render_agent_tool_error(
            None,
            &format!(
                "Invalid input: target_count {} exceeds maximum of 50",
                input.target_count
            ),
        );
    }
    if input.slots.len() != input.target_count {
        return render_agent_tool_error(
            None,
            &format!(
                "Invalid input: target_count {} requires exactly {} slots, got {}",
                input.target_count,
                input.target_count,
                input.slots.len()
            ),
        );
    }

    let group_id = match input.group_id.as_deref().map(str::trim) {
        Some("") => {
            return render_agent_tool_error(None, "Invalid input: group_id must be non-empty");
        }
        Some(group_id) => group_id.to_string(),
        None => next_fanout_group_id(ctx),
    };
    let title = input
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or(&group_id)
        .to_string();

    // Validate all slots before spawning any.
    let mut seen_slot_ids = HashSet::new();
    for (slot_index, slot) in input.slots.iter_mut().enumerate() {
        if let Some(slot_id) = slot.slot_id.as_mut() {
            let trimmed = slot_id.trim();
            if trimmed.is_empty() {
                return render_agent_tool_error(
                    None,
                    &format!("Invalid input: slots[{slot_index}].id must be non-empty"),
                );
            }
            if trimmed.len() != slot_id.len() {
                *slot_id = trimmed.to_string();
            }
            let slot_id = slot_id.clone();
            if !seen_slot_ids.insert(slot_id.clone()) {
                return render_agent_tool_error(
                    None,
                    &format!(
                        "Invalid input: slots[{slot_index}].id '{}' is duplicated",
                        slot_id
                    ),
                );
            }
        }
        if slot.description.trim().is_empty() {
            return render_agent_tool_error(
                None,
                &format!("Invalid input: slots[{slot_index}].description must be non-empty"),
            );
        }
        if slot.prompt.trim().is_empty() {
            return render_agent_tool_error(
                None,
                &format!("Invalid input: slots[{slot_index}].prompt must be non-empty"),
            );
        }
    }
    if let Some(existing) = ctx.spawner.fanout_group_for_parent_run(&ctx.run_id).await {
        let same_start = existing.group_id == group_id
            || input._tool_call_id.as_deref().is_some_and(|tool_call_id| {
                existing.created_by_tool_use_id.as_deref() == Some(tool_call_id)
            });
        if !same_start {
            return render_agent_tool_error(
                None,
                &format!(
                    "parent run '{}' already started fanout group '{}' with fixed target_count {}; a parent run may start only one fanout group",
                    ctx.run_id, existing.group_id, existing.target_count
                ),
            );
        }
        if existing.is_terminal() {
            return render_agent_fanout_results(
                ctx,
                &existing.group_id,
                input._tool_call_id,
                FanoutResultReadOptions::default(),
            )
            .await;
        }
        return json!({
            "status": "started",
            "group_id": existing.group_id,
            "title": existing.title,
            "target_count": existing.target_count,
            "fanout": fanout_group_to_json(&existing),
            "idempotent_replay": true,
            "instruction": "This fanout start was already accepted. Observe the existing group with agent_fanout.get_results; no replacement agents were launched."
        })
        .to_string();
    }
    let slots = std::mem::take(&mut input.slots);
    let tool_call_id = input._tool_call_id.clone();
    if let Err(error) = ctx
        .spawner
        .declare_fanout_group(
            &group_id,
            &title,
            input.target_count,
            tool_call_id.as_deref(),
            &ctx.run_id,
        )
        .await
    {
        return render_agent_tool_error(None, &error.to_string());
    }

    // Spawn all slots concurrently — no head-of-line blocking.
    let futs: Vec<_> = slots
        .into_iter()
        .enumerate()
        .map(|(slot_index, slot)| {
            let slot_id = slot.slot_id.clone();
            let spawn_args = fanout_slot_spawn_args(
                &input,
                slot,
                &group_id,
                &title,
                input.target_count,
                slot_index,
                tool_call_id.as_deref(),
            );
            Box::pin(async move {
                let rendered = handle_agent_spawn_action(&spawn_args, Some(ctx)).await;
                let rendered_value = serde_json::from_str::<Value>(&rendered)
                    .unwrap_or_else(|_| json!({ "status": "failed", "error": rendered }));
                json!({
                    "slot_index": slot_index,
                    "id": slot_id,
                    "agent_id": rendered_value.get("agent_id").cloned().unwrap_or(Value::Null),
                    "run_id": rendered_value.get("run_id").cloned().unwrap_or(Value::Null),
                    "status": rendered_value.get("status").cloned().unwrap_or(Value::Null),
                    "finish_reason": rendered_value.get("finish_reason").cloned().unwrap_or(Value::Null),
                    "error": rendered_value.get("error").cloned().unwrap_or(Value::Null),
                    "transcript_location": rendered_value.get("transcript_location").cloned().unwrap_or(Value::Null),
                })
            })
        })
        .collect();
    let mut agents: Vec<Value> = join_all(futs).await;
    // Restore slot-index order.
    agents.sort_by_key(|v| v.get("slot_index").and_then(Value::as_u64).unwrap_or(0));
    // `Launched` is possible only after the user explicitly promotes the
    // foreground group with Ctrl+B. Return a stable handoff receipt instead
    // of pretending those detached children already produced results.
    let any_launched = agents
        .iter()
        .any(|agent| agent.get("status").and_then(Value::as_str) == Some("launched"));
    let terminal_causes = FanoutStartTerminalCauses::from_agents(&agents);
    if any_launched {
        let group = find_fanout_group(ctx, &group_id).await;
        let mut resp = json!({
            "status": "started",
            "group_id": group_id,
            "title": title,
            "target_count": input.target_count,
            "transcript_location": ctx.transcript_location.wire_value(),
            "agents": agents,
            "fanout": group.as_ref().map(fanout_group_to_json).unwrap_or(Value::Null),
            "delivery": "explicit_background_handoff",
            "instruction": "The user moved this fanout group to the background. Do not claim completion or analyze individual slot events. The runtime will surface one terminal group update; live progress remains available with Shift+Down.",
        });
        if terminal_causes.has_stopped_slots() {
            let obj = resp.as_object_mut().unwrap();
            terminal_causes.insert_json_fields(obj);
            obj.insert(
                "instruction".into(),
                json!("Some fanout slots already stopped before the group fully launched. Do not retry or spawn replacements. Use agent_fanout(action='get_results', group_id=...) to collect completed or partial results when ready."),
            );
        }
        // If any slot failed to spawn synchronously, inject anti-respawn
        // instruction so the LLM doesn't try to "fix" partial starts.
        let any_spawn_failed = agents.iter().any(|a| {
            a.get("status")
                .and_then(Value::as_str)
                .is_some_and(|s| s == "failed")
        });
        if any_spawn_failed && !terminal_causes.has_stopped_slots() {
            resp.as_object_mut().unwrap().insert(
                "instruction".into(),
                json!("Some agents failed to spawn. Do NOT retry or spawn replacements. Use agent_fanout(action='get_results', group_id=...) to collect partial results when ready."),
            );
        }
        return resp.to_string();
    }

    // Every accepted foreground slot has now reached a stable execution
    // boundary. Return one canonical aggregate for successful, partial,
    // failed, cancelled, timed-out, and spawn-rejected groups alike. A bad
    // child result must not force a second model round merely to collect the
    // evidence already owned by the runtime.
    render_agent_fanout_results(
        ctx,
        &group_id,
        tool_call_id,
        FanoutResultReadOptions::default(),
    )
    .await
}

async fn handle_agent_fanout_get_results_action(
    args: &Value,
    ctx: Option<&AgentToolContext>,
) -> String {
    if let Err(e) = validate_agent_fanout_group_shape(
        args,
        "get_results",
        FANOUT_GET_RESULTS_FIELDS,
        FANOUT_GET_RESULTS_SHAPE,
    ) {
        return render_agent_tool_error(None, &format!("Invalid input: {e}"));
    }
    let ctx = match ctx {
        Some(c) => c,
        None => {
            return render_agent_runtime_binding_error("agent_fanout", "get_results");
        }
    };
    let input: AgentFanoutGroupInput = match serde_json::from_value(args.clone()) {
        Ok(input) => input,
        Err(e) => {
            return render_agent_tool_error(
                None,
                &format!(
                    "Invalid input for agent_fanout.get_results: {e}. {FANOUT_GET_RESULTS_SHAPE}"
                ),
            );
        }
    };
    let group_id = input.group_id.trim();
    if group_id.is_empty() {
        return render_agent_tool_error(None, "Invalid input: group_id must be non-empty");
    }
    let read_options = match FanoutResultReadOptions::from_group_input(&input) {
        Ok(read_options) => read_options,
        Err(error) => return render_agent_tool_error(None, &format!("Invalid input: {error}")),
    };
    render_agent_fanout_results(ctx, group_id, input._tool_call_id, read_options).await
}

#[derive(Debug, Clone, Copy)]
struct FanoutResultReadOptions {
    slot_index: Option<usize>,
    offset: usize,
    max_bytes: usize,
}

impl Default for FanoutResultReadOptions {
    fn default() -> Self {
        Self {
            slot_index: None,
            offset: 0,
            max_bytes: FANOUT_RESULT_DEFAULT_MAX_BYTES,
        }
    }
}

impl FanoutResultReadOptions {
    fn is_default(self) -> bool {
        self.slot_index.is_none()
            && self.offset == 0
            && self.max_bytes == FANOUT_RESULT_DEFAULT_MAX_BYTES
    }

    fn from_group_input(input: &AgentFanoutGroupInput) -> Result<Self, String> {
        if input.offset.unwrap_or(0) > 0 && input.slot_index.is_none() {
            return Err(
                "`offset` requires `slot_index`; aggregate fanout summaries are not byte-paged. \
                 Use `slot_index` to read a specific slot result window."
                    .to_string(),
            );
        }
        let requested_max = input.max_bytes.unwrap_or(FANOUT_RESULT_DEFAULT_MAX_BYTES);
        Ok(Self {
            slot_index: input.slot_index,
            offset: input.offset.unwrap_or(0),
            max_bytes: requested_max.clamp(1, FANOUT_RESULT_MAX_BYTES),
        })
    }
}

async fn render_agent_fanout_results(
    ctx: &AgentToolContext,
    group_id: &str,
    tool_call_id: Option<String>,
    read_options: FanoutResultReadOptions,
) -> String {
    if read_options.is_default()
        && let Some(cached) = ctx.spawner.cached_terminal_fanout_result(group_id).await
    {
        return cached;
    }
    if let Err(error) = ctx.spawner.reconcile_durable_agent_runs().await {
        tracing::warn!(
            target: "fanout",
            %group_id,
            %error,
            "durable fanout reconciliation failed; returning the last confirmed observation"
        );
    }
    let Some(group) = find_fanout_group(ctx, group_id).await else {
        return render_agent_tool_error(None, &format!("Unknown fanout group_id: {group_id}"));
    };
    if let Some(slot_index) = read_options.slot_index
        && !group.slots.iter().any(|slot| slot.slot_index == slot_index)
    {
        return render_agent_tool_error(
            None,
            &format!(
                "Invalid input: slot_index {slot_index} is outside target_count {}",
                group.target_count
            ),
        );
    }

    let mut results: Vec<Value> = Vec::with_capacity(group.slots.len());
    let mut futs: Vec<_> = Vec::new();

    for slot in &group.slots {
        if read_options
            .slot_index
            .is_some_and(|slot_index| slot.slot_index != slot_index)
        {
            continue;
        }
        let Some(agent_id) = slot.agent_id.as_deref() else {
            results.push(json!({
                "slot_index": slot.slot_index,
                "id": &slot.slot_id,
                "status": fanout_slot_status_label(slot.status),
                "error": slot.terminal_reason,
            }));
            continue;
        };
        let agent_id = agent_id.to_string();
        let slot_index = slot.slot_index;
        let slot_id = slot.slot_id.clone();
        let tool_call_id = tool_call_id.clone();
        let group_id = group_id.to_string();
        let mut get_args = json!({ "agent_id": agent_id });
        if let Some(tool_call_id) = tool_call_id {
            get_args
                .as_object_mut()
                .expect("get_result args object")
                .insert("_tool_call_id".to_string(), Value::String(tool_call_id));
        }
        futs.push(Box::pin(async move {
            let rendered = handle_agent_get_result_action_inner(&get_args, Some(ctx), false).await;
            let mut value = serde_json::from_str::<Value>(&rendered)
                .unwrap_or_else(|_| json!({ "status": "failed", "error": rendered }));
            let window = window_fanout_agent_result(
                &mut value,
                &group_id,
                slot_index,
                read_options.offset,
                read_options.max_bytes,
            );
            // The group envelope below is authoritative for fanout state.
            // `agent.get_result` also attaches a full fanout summary to every
            // child, which made a three-slot read repeat the same slot table
            // three additional times. Besides wasting prompt budget, large
            // terminal reads crossed the artifact threshold and forced the
            // parent into several avoidable recovery turns. Preserve the
            // child result itself and its result window, but carry group state
            // exactly once.
            if let Some(object) = value.as_object_mut() {
                object.remove("fanout");
            }
            let needs_recovery = agent_tool_result_needs_recovery(&value);
            let mut item = json!({
                "slot_index": slot_index,
                "id": slot_id,
                "agent_id": agent_id,
                "result": value,
            });
            if needs_recovery {
                let object = item.as_object_mut().expect("slot result item object");
                let resume_existing_agent_id = object
                    .get("agent_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                object.insert(
                    "recovery".into(),
                    json!({
                        "resume_existing_agent_id": resume_existing_agent_id,
                        "rerun_policy": "resume_existing_agent_or_report_incomplete",
                        "do_not_spawn_replacement": true,
                    }),
                );
            }
            if let Some(window) = window {
                let object = item.as_object_mut().expect("slot result item object");
                object.insert("result_bytes".into(), json!(window.total_bytes));
                object.insert("result_start_offset".into(), json!(window.start));
                object.insert("result_end_offset".into(), json!(window.end));
                object.insert("result_truncated".into(), json!(window.truncated));
                if let Some(next_call) = window.next_call {
                    object.insert("next_call".into(), json!(next_call));
                }
            }
            item
        }));
    }

    // Query all agent results concurrently.
    let mut concurrent: Vec<Value> = join_all(futs).await;
    results.append(&mut concurrent);
    // Restore slot-index order.
    results.sort_by_key(|v| v.get("slot_index").and_then(Value::as_u64).unwrap_or(0));

    // Enforce total aggregate byte budget: if the combined results exceed
    // MAX_FANOUT_AGGREGATE_BYTES, re-truncate per-slot proportionally.
    let serialized_total: usize = results.iter().map(|v| v.to_string().len()).sum();
    if serialized_total > MAX_FANOUT_AGGREGATE_BYTES && !results.is_empty() {
        let per_slot_budget = MAX_FANOUT_AGGREGATE_BYTES / results.len();
        for item in &mut results {
            if let Some(result_obj) = item.get("result") {
                let result_str = result_obj.to_string();
                if result_str.len() > per_slot_budget {
                    let truncated = truncate_str_at_char_boundary(&result_str, per_slot_budget);
                    item["result"] = json!(format!(
                        "{}\n\n[truncated — {} bytes total; use agent_fanout(action='get_results', group_id='{}', slot_index={}, offset=0, max_bytes={}) for a bounded slot window]",
                        truncated,
                        result_str.len(),
                        group_id,
                        item.get("slot_index").and_then(Value::as_u64).unwrap_or(0),
                        FANOUT_RESULT_MAX_BYTES,
                    ));
                }
            }
        }
    }

    let updated = find_fanout_group(ctx, group_id).await.unwrap_or(group);
    let summary = updated.summary();
    let incomplete_result_count = results
        .iter()
        .filter(|item| fanout_result_item_has_terminal_incomplete(item))
        .count();
    let all_slots_delivered = summary.completed == summary.target_count;
    let incomplete_slot_count = summary.target_count.saturating_sub(summary.completed);
    let has_failures = summary.failed > 0
        || summary.interrupted > 0
        || summary.spawn_rejected > 0
        || summary.timed_out > 0
        || summary.cancelled_by_user > 0
        || summary.cancelled_by_parent_budget > 0;
    let work_status = if summary.active > 0 {
        WorkUnitStatus::Running
    } else if has_failures {
        WorkUnitStatus::CompletedWithIssues
    } else {
        WorkUnitStatus::Completed
    };
    let observation_mode = if read_options.slot_index.is_some() || read_options.offset > 0 {
        WorkUnitObservationMode::Historical
    } else {
        WorkUnitObservationMode::Current
    };
    let work_observation = WorkUnitObservation::new(
        group_id,
        "agent_fanout",
        work_status,
        updated.revision.to_string(),
        observation_mode,
    )
    .expect("fanout groups have non-empty identities and revisions")
    .with_wake_policy(WorkUnitWakePolicy::OnTerminal);
    let mut response = json!({
        "status": fanout_get_results_status_label(&updated),
        "group_id": group_id,
        "title": updated.title,
        "target_count": updated.target_count,
        "active": summary.active,
        "terminal": summary.terminal,
        "completed": summary.completed,
        "failed": summary.failed,
        "interrupted": summary.interrupted,
        "cancelled_by_user": summary.cancelled_by_user,
        "cancelled_by_parent_budget": summary.cancelled_by_parent_budget,
        "timed_out": summary.timed_out,
        "spawn_rejected": summary.spawn_rejected,
        "transcript_location": ctx.transcript_location.wire_value(),
        "delivery_contract": "Results are bounded for prompt safety. Use results[].next_call, or agent_fanout(action='get_results', group_id=..., slot_index=N, offset=BYTE_OFFSET, max_bytes=BYTES), to read additional slot output. Do not search for or copy physical filesystem paths or runtime-owned tool-result artifacts.",
        "recovery": {
            "result_ref": format!("agent_fanout:{group_id}"),
            "task_output_id": group_id,
            "get_results_call": format!("agent_fanout(action='get_results', group_id='{group_id}')"),
            "task_output_call": format!("task_output(task_id='{group_id}')"),
            "active_task_list_empty_does_not_mean_results_missing": true,
            "do_not_rerun_when_user_asks_for_results": true,
        },
        "result_read": {
            "slot_index": read_options.slot_index,
            "offset": read_options.offset,
            "max_bytes": read_options.max_bytes,
        },
        "fanout": fanout_group_to_json(&updated),
        "provenance": {
            "source": "fanout_group",
            "target_count": summary.target_count,
            "complete_deliverables": summary.completed,
            "incomplete_slots": incomplete_slot_count,
            "all_slots_delivered": all_slots_delivered,
            "observed_terminal_incomplete_results": incomplete_result_count,
            "attribution_contract": "Attribute a finding to a child only when that child's returned result contains it. If all_slots_delivered is false, disclose the completion ratio and label any independent parent analysis as parent synthesis rather than fanout consensus.",
        },
        "results": results,
    });
    let obj = response.as_object_mut().unwrap();
    obj.insert(
        WORK_UNIT_OBSERVATION_FIELD.to_string(),
        work_observation.to_value(),
    );
    if incomplete_result_count > 0 {
        obj.insert("incomplete_results".into(), json!(incomplete_result_count));
        if let Some(recovery) = obj.get_mut("recovery").and_then(Value::as_object_mut) {
            recovery.insert("resume_existing_work_before_rerun".into(), json!(true));
            recovery.insert(
                "rerun_policy".into(),
                json!("resume_existing_agents_or_report_incomplete; do_not_respawn_slots"),
            );
        }
    }
    // Anti-respawn instruction: prevent LLM from spawning additional agents
    // to retry failed slots. The fanout group is a fixed-size contract;
    // retries inflate the group and corrupt accounting.
    if summary.active > 0 {
        obj.insert(
            "instruction".into(),
            json!(
                "This explicitly backgrounded fanout group is still running. Do not busy-poll get_results or analyze individual slot events; the runtime will surface one terminal group update. Live progress remains available with Shift+Down."
            ),
        );
    } else if has_failures {
        obj.insert(
            "instruction".into(),
            json!(format!(
                "Do NOT retry, respawn, or spawn additional agents to replace failed/interrupted/cancelled slots. The fanout group has a fixed target_count and adding agents corrupts accounting. Exactly {}/{} slots produced complete deliverables. Disclose that ratio; do not describe incomplete slots as reviewers or validators, and label independent parent analysis as parent synthesis. Work with the results you have, or ask the user how to proceed.",
                summary.completed, summary.target_count
            )),
        );
    } else if summary.active == 0 && summary.terminal == summary.target_count {
        obj.insert(
            "instruction".into(),
            json!(
                "Fanout target_count is complete. Do not call agent(action='spawn') to add, retry, or replace agents in this turn. Present the collected results; ask the user before starting any additional fanout."
            ),
        );
    }
    let rendered = serde_json::to_string_pretty(&response).unwrap_or_else(|_| response.to_string());
    if read_options.is_default() && updated.is_terminal() {
        ctx.spawner
            .cache_terminal_fanout_result(group_id, rendered.clone())
            .await;
    }
    rendered
}

async fn handle_agent_fanout_stop_slot_action(
    args: &Value,
    ctx: Option<&AgentToolContext>,
) -> String {
    if let Err(e) = validate_agent_fanout_group_shape(
        args,
        "stop_slot",
        FANOUT_STOP_SLOT_FIELDS,
        FANOUT_STOP_SLOT_SHAPE,
    )
    .and_then(|_| {
        let object = args.as_object().expect("validated object");
        validate_required_field(
            object,
            "slot_index",
            "agent_fanout.stop_slot",
            FANOUT_STOP_SLOT_SHAPE,
        )
    }) {
        return render_agent_tool_error(None, &format!("Invalid input: {e}"));
    }
    let ctx = match ctx {
        Some(c) => c,
        None => {
            return render_agent_runtime_binding_error("agent_fanout", "stop_slot");
        }
    };
    let input: AgentFanoutStopSlotInput = match serde_json::from_value(args.clone()) {
        Ok(input) => input,
        Err(e) => {
            return render_agent_tool_error(
                None,
                &format!("Invalid input for agent_fanout.stop_slot: {e}. {FANOUT_STOP_SLOT_SHAPE}"),
            );
        }
    };
    let group_id = input.group_id.trim();
    if group_id.is_empty() {
        return render_agent_tool_error(None, "Invalid input: group_id must be non-empty");
    }
    let Some(group) = find_fanout_group(ctx, group_id).await else {
        return render_agent_tool_error(None, &format!("Unknown fanout group_id: {group_id}"));
    };
    let Some(slot) = group
        .slots
        .iter()
        .find(|slot| slot.slot_index == input.slot_index)
    else {
        return render_agent_tool_error(
            None,
            &format!(
                "Invalid input: slot_index {} is outside target_count {}",
                input.slot_index, group.target_count
            ),
        );
    };
    let slot_status = fanout_slot_status_label(slot.status);
    let slot_id = slot.slot_id.clone();
    let terminal_reason = slot.terminal_reason.clone();
    let Some(agent_id) = slot.agent_id.clone() else {
        return json!({
            "status": "not_stoppable",
            "reason": "no_accepted_agent",
            "group_id": group_id,
            "slot_index": input.slot_index,
            "id": slot_id,
            "slot_status": slot_status,
            "terminal_reason": terminal_reason,
            "fanout": fanout_group_to_json(&group),
        })
        .to_string();
    };
    if slot.status.is_terminal() {
        return json!({
            "status": "not_stoppable",
            "reason": "already_terminal",
            "group_id": group_id,
            "slot_index": input.slot_index,
            "id": slot_id,
            "agent_id": agent_id,
            "slot_status": slot_status,
            "terminal_reason": terminal_reason,
            "fanout": fanout_group_to_json(&group),
        })
        .to_string();
    }

    let stopped = ctx
        .spawner
        .cancel_agent(&agent_id, "user-requested via agent_fanout.stop_slot")
        .await;
    let updated = find_fanout_group(ctx, group_id).await.unwrap_or(group);
    let updated_slot = updated
        .slots
        .iter()
        .find(|slot| slot.slot_index == input.slot_index);
    json!({
        "status": if stopped { "stopped" } else { "not_stopped" },
        "group_id": group_id,
        "slot_index": input.slot_index,
        "id": updated_slot
            .and_then(|slot| slot.slot_id.as_deref())
            .or(slot_id.as_deref()),
        "agent_id": agent_id,
        "slot_status": updated_slot
            .map(|slot| fanout_slot_status_label(slot.status))
            .unwrap_or(slot_status),
        "terminal_reason": updated_slot
            .and_then(|slot| slot.terminal_reason.as_deref())
            .or(terminal_reason.as_deref()),
        "fanout": fanout_group_to_json(&updated),
    })
    .to_string()
}

fn fanout_slot_spawn_args(
    input: &AgentFanoutStartInput,
    slot: AgentFanoutStartSlot,
    group_id: &str,
    group_title: &str,
    target_count: usize,
    slot_index: usize,
    tool_call_id: Option<&str>,
) -> Value {
    let mut value = json!({
        "action": "spawn",
        "description": slot.description,
        "prompt": slot.prompt,
        "fanout_group_id": group_id,
        "fanout_group_title": group_title,
        "fanout_target_count": target_count,
        "fanout_slot_index": slot_index,
    });
    let object = value.as_object_mut().expect("object");
    let defaults = input.defaults.as_ref();
    let effective_max_turns = fanout_effective_max_turns(&slot, defaults);
    insert_optional_string(
        object,
        "agent_type",
        slot.agent_type
            .or_else(|| defaults.and_then(|d| d.agent_type.clone())),
    );
    insert_optional_string(
        object,
        "model",
        slot.model
            .or_else(|| defaults.and_then(|d| d.model.clone())),
    );
    insert_optional_u32(object, "max_turns", effective_max_turns);
    insert_optional_u32(
        object,
        "max_output_tokens",
        slot.max_output_tokens
            .or_else(|| defaults.and_then(|d| d.max_output_tokens)),
    );
    insert_optional_string(
        object,
        "complexity",
        slot.complexity
            .or_else(|| defaults.and_then(|d| d.complexity.clone())),
    );
    insert_optional_bool(
        object,
        "isolated",
        slot.isolated.or_else(|| defaults.and_then(|d| d.isolated)),
    );
    insert_optional_string(object, "fanout_slot_id", slot.slot_id);
    if let Some(allowed_tools) = slot
        .allowed_tools
        .or_else(|| defaults.and_then(|d| d.allowed_tools.clone()))
    {
        object.insert("allowed_tools".to_string(), json!(allowed_tools));
    }
    if let Some(tool_call_id) = tool_call_id {
        object.insert(
            "_tool_call_id".to_string(),
            Value::String(tool_call_id.to_string()),
        );
    }
    value
}

fn fanout_effective_max_turns(
    slot: &AgentFanoutStartSlot,
    defaults: Option<&AgentFanoutDefaults>,
) -> Option<u32> {
    slot.max_turns
        .or_else(|| defaults.and_then(|defaults| defaults.max_turns))
}

fn insert_optional_string(
    object: &mut serde_json::Map<String, Value>,
    key: &str,
    value: Option<String>,
) {
    if let Some(value) = value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        object.insert(key.to_string(), Value::String(value));
    }
}

fn insert_optional_u32(object: &mut serde_json::Map<String, Value>, key: &str, value: Option<u32>) {
    if let Some(value) = value {
        object.insert(key.to_string(), json!(value));
    }
}

fn insert_optional_bool(
    object: &mut serde_json::Map<String, Value>,
    key: &str,
    value: Option<bool>,
) {
    if let Some(value) = value {
        object.insert(key.to_string(), json!(value));
    }
}

fn next_fanout_group_id(ctx: &AgentToolContext) -> String {
    let id = NEXT_FANOUT_GROUP_ID.fetch_add(1, Ordering::Relaxed);
    format!("{}-fanout-{id}", ctx.run_id)
}

async fn find_fanout_group(
    ctx: &AgentToolContext,
    group_id: &str,
) -> Option<AgentFanoutGroupProjection> {
    ctx.spawner
        .list_fanout_groups()
        .await
        .into_iter()
        .find(|group| group.group_id == group_id)
}

fn fanout_group_to_json(group: &AgentFanoutGroupProjection) -> Value {
    let summary = group.summary();
    json!({
        "group_id": group.group_id,
        "title": group.title,
        "parent_run_id": group.parent_run_id,
        "target_count": summary.target_count,
        "revision": group.revision,
        "status": group.status.as_str(),
        "summary": group.summary_sentence(),
        "accepted": summary.accepted,
        "active": summary.active,
        "terminal": summary.terminal,
        "completed": summary.completed,
        "interrupted": summary.interrupted,
        "failed": summary.failed,
        "cancelled_by_user": summary.cancelled_by_user,
        "cancelled_by_parent_budget": summary.cancelled_by_parent_budget,
        "timed_out": summary.timed_out,
        "spawn_rejected": summary.spawn_rejected,
        "collected": summary.collected,
        "uncollected": summary.uncollected,
        "slots": group.slots.iter().map(|slot| json!({
            "slot_index": slot.slot_index,
            "id": &slot.slot_id,
            "role": slot.role,
            "requested_description": slot.requested_description,
            "agent_id": &slot.agent_id,
            "run_id": &slot.run_id,
            "status": fanout_slot_status_label(slot.status),
            "result_collected": slot.result_collected,
            "terminal_reason": &slot.terminal_reason,
        })).collect::<Vec<_>>(),
    })
}

fn fanout_get_results_status_label(group: &AgentFanoutGroupProjection) -> &'static str {
    let summary = group.summary();
    if summary.active > 0 {
        "incomplete"
    } else if summary.spawn_rejected > 0
        || summary.cancelled_by_parent_budget > 0
        || summary.failed > 0
        || summary.timed_out > 0
        || summary.interrupted > 0
        || summary.cancelled_by_user > 0
    {
        "completed_with_issues"
    } else {
        "completed"
    }
}

fn fanout_result_item_has_terminal_incomplete(item: &Value) -> bool {
    item.get("status")
        .and_then(Value::as_str)
        .is_some_and(fanout_slot_status_is_recoverable_issue)
        || item
            .get("result")
            .is_some_and(agent_tool_result_needs_recovery)
}

fn fanout_slot_status_label(status: AgentFanoutSlotStatus) -> &'static str {
    status.as_str()
}

/// Handle `agent(action='spawn')`.
pub async fn handle_agent_spawn_action(args: &Value, ctx: Option<&AgentToolContext>) -> String {
    let mut input: SpawnAgentInput = match normalize_agent_spawn_args(args)
        .and_then(|patched_args| serde_json::from_value(patched_args).map_err(|e| e.to_string()))
    {
        Ok(i) => i,
        Err(e) => {
            return render_agent_tool_error(None, &format!("Invalid input: {e}"));
        }
    };

    let ctx = match ctx {
        Some(c) => c,
        None => {
            return render_agent_runtime_binding_error("agent", "spawn");
        }
    };

    if input.model.is_none() {
        input.model = ctx.current_model.clone();
    }
    // Structured concurrency is the public default. The spawner waits for the
    // child result while still streaming live progress and accepting UI
    // control. Only an explicit user Ctrl+B promotion can flip the runtime
    // state to background and wake this wait with `Launched`.
    if let Err(e) = input.validate_fanout_metadata() {
        return render_agent_tool_error(None, &format!("Invalid input: {e}"));
    }

    let mut inherited_permissions = ctx.inherited_permissions.clone();
    inherited_permissions.is_background = input.run_in_background;

    // Propagate delegation chain: child_chain = parent_chain + parent_agent_id.
    // This enables circular delegation detection across agent spawn hops
    // (e.g., A spawns B, B spawns C, C tries to spawn A → detected).
    let mut child_delegation_chain = ctx.delegation_chain.clone();
    child_delegation_chain.push(ctx.agent_id.clone());

    // CLI parents have a turn-scoped execution run_id but a stable root
    // mailbox. Record that relationship before the child is registered so
    // terminal/checkpoint messages never target an address that disappears
    // with the spawning turn. Server/child contexts normally resolve to the
    // same run_id and therefore need no alias.
    let mailbox_router = ctx.spawner.mailbox_router();
    if let Some(parent_mailbox) = mailbox_router
        .registered_address_for_agent(&ctx.agent_id)
        .await
    {
        mailbox_router
            .record_parent_delivery_alias(&ctx.run_id, &parent_mailbox)
            .await;
    }

    let spawn_ctx = SpawnContext {
        parent_run_id: ctx.run_id.clone(),
        parent_agent_id: ctx.agent_id.clone(),
        recursion_depth: ctx.recursion_depth,
        parent_is_fork_child: ctx.is_fork_child,
        working_dir: ctx.working_dir.clone(),
        inherited_permissions,
        inherited_skills: ctx.active_skills.clone(),
        live_event_sink: ctx.live_event_sink.clone(),
        client_tool_delivery_tx: ctx.client_tool_delivery_tx.clone(),
        trace_context: ctx.trace_context.clone(),
        execution_metadata: ctx.execution_metadata.clone(),
        spawn_tool_call_id: args
            .get("_tool_call_id")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        delegation_chain: child_delegation_chain,
    };

    match ctx.spawner.spawn(input, &spawn_ctx).await {
        Ok(output) => render_spawn_agent_output(output, ctx.transcript_location),
        Err(SpawnError::ExecutorUnavailable) => {
            render_agent_runtime_binding_error("agent", "spawn")
        }
        Err(e) => render_agent_tool_error(None, &e.to_string()),
    }
}

fn non_empty_string(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Truncate `s` to at most `max_bytes` bytes, landing on a UTF-8 char boundary.
/// Avoids `byte index N is not a char boundary` panic on multi-byte input.
/// Returns `s` unchanged if it already fits.
fn truncate_str_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    // Walk char boundaries from the start, take the largest index <= max_bytes.
    s.char_indices()
        .take_while(|(i, _)| *i <= max_bytes)
        .last()
        .map(|(i, _)| &s[..i])
        .unwrap_or("")
}

#[derive(Debug, Clone)]
struct FanoutResultWindow {
    total_bytes: usize,
    start: usize,
    end: usize,
    truncated: bool,
    next_call: Option<String>,
}

fn floor_char_boundary(s: &str, mut index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    while index > 0 && !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn window_fanout_agent_result(
    value: &mut Value,
    group_id: &str,
    slot_index: usize,
    offset: usize,
    max_bytes: usize,
) -> Option<FanoutResultWindow> {
    let result_text = value.get("result").and_then(Value::as_str)?.to_string();
    let total_bytes = result_text.len();
    let start = floor_char_boundary(&result_text, offset.min(total_bytes));
    let end = floor_char_boundary(&result_text, (start + max_bytes).min(total_bytes));
    let truncated = start > 0 || end < total_bytes;
    if truncated {
        value["result"] = json!(result_text[start..end].to_string());
    }
    let next_call = if end < total_bytes {
        Some(format!(
            "agent_fanout(action='get_results', group_id='{group_id}', slot_index={slot_index}, offset={end}, max_bytes={max_bytes})"
        ))
    } else {
        None
    };
    Some(FanoutResultWindow {
        total_bytes,
        start,
        end,
        truncated,
        next_call,
    })
}

/// Normalize raw `agent(action='spawn', ...)` arguments into
/// [`SpawnAgentInput`] wire shape.
///
/// The shared runtime rejects batch, wrapper, deprecated, and alias shapes
/// explicitly before deserializing the canonical spawn payload.
///
pub fn normalize_agent_spawn_args(args: &Value) -> Result<Value, String> {
    let mut patched_args = args.clone();
    let obj = patched_args
        .as_object_mut()
        .ok_or_else(|| "spawn input must be a JSON object".to_string())?;

    if obj.contains_key("spawn") {
        return Err("invalid `spawn` wrapper for `agent(action='spawn')`: use top-level fields, for example `agent(action='spawn', description='...', prompt='...')`."
            .to_string());
    }

    if obj.contains_key("agents") {
        return Err(
            "unsupported `agents` payload for `agent(action='spawn')`: each \
             `agent(action='spawn', ...)` call launches exactly one child. \
             Use `agent_fanout(action='start', target_count=N, slots=[...])` \
             for atomic parallel fan-out."
                .to_string(),
        );
    }

    if obj.contains_key("task") {
        return Err(
            "unsupported deprecated `task` field for `agent(action='spawn')`. \
             Use top-level `prompt` for the full child task brief and \
             `description` for the short UI summary."
                .to_string(),
        );
    }

    if obj.contains_key("type") {
        return Err(
            "unsupported `type` field for `agent(action='spawn')`. Use canonical `agent_type`."
                .to_string(),
        );
    }

    if obj.contains_key("inherit_context") {
        return Err(
            "unsupported `inherit_context` field for `agent(action='spawn')`. Use canonical `inherit_prefix`."
                .to_string(),
        );
    }

    if obj.contains_key("agent_id") {
        return Err("unsupported `agent_id` field for `agent(action='spawn')`. `agent_id` is only valid for `agent(action='get_result')`; the runtime generates it after spawn."
            .to_string());
    }

    if obj.contains_key("run_in_background") {
        return Err(
            "unsupported `run_in_background` field for `agent(action='spawn')`: foreground fan-in is the safe default and backgrounding is an explicit user control. Omit the field; in the terminal the user can press Ctrl+B while the child is running."
                .to_string(),
        );
    }

    obj.remove("action");
    obj.remove("_tool_call_id");

    let description = non_empty_string(obj.get("description")).map(str::to_string);
    let prompt = non_empty_string(obj.get("prompt")).map(str::to_string);
    if description.is_none() {
        return Err("missing required field `description`".to_string());
    }

    if prompt.is_none() {
        return Err("missing required field `prompt`".to_string());
    }

    Ok(patched_args)
}

/// Handle `agent(action='get_result')`.
pub async fn handle_agent_get_result_action(
    args: &Value,
    ctx: Option<&AgentToolContext>,
) -> String {
    handle_agent_get_result_action_inner(args, ctx, true).await
}

async fn enrich_collected_agent_result(
    rendered: String,
    ctx: &AgentToolContext,
    agent_id: &str,
) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(&rendered) else {
        return rendered;
    };
    let Some(object) = value.as_object_mut() else {
        return rendered;
    };
    if let Some(state) = ctx.spawner.get_agent_state_any(agent_id).await {
        object.insert("run_id".into(), json!(state.run_id));
        object.insert("tool_calls".into(), json!(state.metrics.tool_calls));
        let duration_ms = state
            .ended_at
            .unwrap_or_else(std::time::SystemTime::now)
            .duration_since(state.started_at)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        object.insert("duration_ms".into(), json!(duration_ms));
    }
    if object.get("status").and_then(Value::as_str) == Some("failed")
        && object.get("finish_reason").and_then(Value::as_str) == Some("executor_dropped")
    {
        object.insert("diagnostic".into(), json!("executor_dropped"));
        object.insert("retryable".into(), json!(false));
        object.insert(
            "instruction".into(),
            json!(
                "The child run was scheduled but its completion payload was lost. Do not retry the agent spawn or create a replacement sub-agent; that could duplicate side effects. Report the incomplete child result or continue with currently bound tools."
            ),
        );
    }
    serde_json::to_string(&value).unwrap_or(rendered)
}

async fn handle_agent_get_result_action_inner(
    args: &Value,
    ctx: Option<&AgentToolContext>,
    reconcile_durable: bool,
) -> String {
    let agent_id = match args.get("agent_id").and_then(Value::as_str).map(str::trim) {
        Some(id) if !id.is_empty() => id,
        None => {
            return render_agent_tool_error(None, "Missing required field: agent_id");
        }
        Some(_) => {
            return render_agent_tool_error(None, "Invalid agent_id: must be a non-empty string");
        }
    };

    let ctx = match ctx {
        Some(c) => c,
        None => {
            return render_agent_runtime_binding_error("agent", "get_result");
        }
    };

    if agent_id.len() > MAX_AGENT_ID_BYTES {
        return render_agent_tool_error(
            None,
            &format!("Invalid agent_id: exceeds {MAX_AGENT_ID_BYTES} bytes"),
        );
    }

    if reconcile_durable && let Err(error) = ctx.spawner.reconcile_durable_agent_runs().await {
        tracing::warn!(
            target: "fanout",
            %agent_id,
            %error,
            "durable child reconciliation failed; returning the last confirmed observation"
        );
    }

    let timeout = AGENT_RESULT_OBSERVE_GRACE;
    match ctx.spawner.wait_for_agent_outcome(agent_id, timeout).await {
        WaitForAgentOutcome::Status(status) => {
            ctx.spawner
                .record_agent_result_collected(
                    &ctx.run_id,
                    &ctx.agent_id,
                    agent_id,
                    args.get("_tool_call_id").and_then(Value::as_str),
                    &status,
                )
                .await;
            let group = ctx.spawner.fanout_group_for_agent(agent_id).await;
            let rendered = render_wait_for_agent_status(agent_id, &status);
            let rendered = enrich_collected_agent_result(rendered, ctx, agent_id).await;
            attach_fanout_to_agent_result(rendered, group)
        }
        WaitForAgentOutcome::TimedOut => {
            let live_status = ctx
                .spawner
                .get_agent_state_any(agent_id)
                .await
                .map(|state| state.status);
            let group = ctx.spawner.fanout_group_for_agent(agent_id).await;
            attach_fanout_to_agent_result(
                render_wait_timeout_outcome(agent_id, live_status.as_ref(), timeout),
                group,
            )
        }
        WaitForAgentOutcome::Unknown => {
            render_unknown_agent_result(agent_id, UNKNOWN_AGENT_ID_ERROR)
        }
    }
}

fn attach_fanout_to_agent_result(
    rendered: String,
    group: Option<AgentFanoutGroupProjection>,
) -> String {
    let Some(group) = group else {
        return rendered;
    };
    let Ok(mut value) = serde_json::from_str::<Value>(&rendered) else {
        return rendered;
    };
    let Some(object) = value.as_object_mut() else {
        return rendered;
    };
    let agent_id = object
        .get("agent_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let slot = agent_id.as_deref().and_then(|agent_id| {
        group
            .slots
            .iter()
            .find(|slot| slot.agent_id.as_deref() == Some(agent_id))
    });
    let slot_index = slot.map(|slot| slot.slot_index);
    let slot_id = slot.and_then(|slot| slot.slot_id.as_deref());
    let summary = group.summary();
    let group_id = group.group_id.clone();
    let summary_sentence = group.summary_sentence();
    object.insert(
        "fanout".to_string(),
        json!({
            "group_id": group_id,
            "target_count": summary.target_count,
            "slot_index": slot_index,
            "id": slot_id,
            "summary": summary_sentence,
            "accepted": summary.accepted,
            "active": summary.active,
            "terminal": summary.terminal,
            "completed": summary.completed,
            "all_slots_delivered": summary.completed == summary.target_count,
            "interrupted": summary.interrupted,
            "failed": summary.failed,
            "cancelled_by_user": summary.cancelled_by_user,
            "cancelled_by_parent_budget": summary.cancelled_by_parent_budget,
            "timed_out": summary.timed_out,
            "spawn_rejected": summary.spawn_rejected,
            "collected": summary.collected,
            "uncollected": summary.uncollected,
        }),
    );
    serde_json::to_string(&value).unwrap_or(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestration::{SpawnAgentExecutor, SpawnRunConfig, SpawnRunResult};
    use crate::server::delegation::engine::DelegationTracker;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[tokio::test]
    async fn spawn_invalid_input_fails_before_context_lookup() {
        let result = handle_agent_spawn_action(&json!({"invalid": "data"}), None).await;
        assert!(result.contains("Invalid input"), "{result}");
        assert!(result.contains("\"status\":\"failed\""), "{result}");
    }

    #[tokio::test]
    async fn spawn_rejects_null_and_non_object_inputs() {
        for args in [Value::Null, json!("spawn"), json!(["prompt"])] {
            let result = handle_agent_spawn_action(&args, None).await;
            assert!(result.contains("Invalid input"), "{result}");
            assert!(result.contains("\"status\":\"failed\""), "{result}");
        }
    }

    #[test]
    fn spawn_arg_normalization_requires_description_even_with_prompt() {
        let err = normalize_agent_spawn_args(&json!({
            "prompt": "Review auth flow"
        }))
        .expect_err("description is canonical and required");
        assert!(err.contains("description"), "{err}");
    }

    #[test]
    fn spawn_arg_normalization_requires_prompt_even_with_description() {
        let err = normalize_agent_spawn_args(&json!({
            "description": "Review auth flow"
        }))
        .expect_err("prompt is canonical and required");
        assert!(err.contains("prompt"), "{err}");
    }

    #[test]
    fn spawn_arg_normalization_rejects_legacy_task_field() {
        let err = normalize_agent_spawn_args(&json!({
            "description": "Audit auth flow",
            "task": "Read src/auth and report token refresh bugs."
        }))
        .expect_err("deprecated task field must be rejected");
        assert!(
            err.contains("deprecated `task` field") && err.contains("prompt"),
            "migration error must tell callers to move to prompt. Got: {err}"
        );
    }

    #[test]
    fn spawn_arg_normalization_rejects_agent_id_field() {
        let err = normalize_agent_spawn_args(&json!({
            "description": "Audit auth flow",
            "prompt": "Read src/auth and report token refresh bugs.",
            "agent_id": "security-review"
        }))
        .expect_err("spawn must reject caller-supplied agent_id");
        assert!(
            err.contains("get_result") && err.contains("runtime generates"),
            "spawn error must name the correct get_result-only contract. Got: {err}"
        );
    }

    #[test]
    fn spawn_arg_normalization_rejects_task_even_when_prompt_is_present() {
        let err = normalize_agent_spawn_args(&json!({
            "description": "Audit auth flow",
            "prompt": "Use the new prompt field.",
            "task": "Do not use this deprecated alias."
        }))
        .expect_err("deprecated task field must stay forbidden even when prompt exists");
        assert!(
            err.contains("deprecated `task` field"),
            "mixed prompt/task payloads must still hard-fail. Got: {err}"
        );
    }

    #[test]
    fn spawn_arg_normalization_never_fabricates_placeholder_prompt() {
        let err = normalize_agent_spawn_args(&json!({ "name": "reviewer-only" }))
            .expect_err("name alone is not enough to spawn a meaningful agent");
        assert!(err.contains("description"), "{err}");
    }

    #[test]
    fn spawn_arg_normalization_rejects_type_alias() {
        let err = normalize_agent_spawn_args(&json!({
            "description": "Audit auth flow",
            "prompt": "Use canonical fields.",
            "type": "task"
        }))
        .expect_err("type alias must be rejected");
        assert!(err.contains("agent_type"), "{err}");
    }

    #[test]
    fn spawn_arg_normalization_rejects_inherit_context_alias() {
        let err = normalize_agent_spawn_args(&json!({
            "description": "Audit auth flow",
            "prompt": "Use canonical fields.",
            "inherit_context": {}
        }))
        .expect_err("inherit_context alias must be rejected");
        assert!(err.contains("inherit_prefix"), "{err}");
    }

    #[test]
    fn spawn_arg_normalization_rejects_agents_batch_payload_with_redirect() {
        let err = normalize_agent_spawn_args(&json!({
            "action": "spawn",
            "agents": [
                {"description": "Review one", "prompt": "p1"},
                {"description": "Review two", "prompt": "p2"}
            ]
        }))
        .expect_err("batch payloads must be rejected with an actionable redirect");
        assert!(err.contains("agents"), "{err}");
        assert!(
            err.contains("agent_fanout") && err.contains("target_count"),
            "error must explain the supported fan-out shape. Got: {err}"
        );
    }

    #[test]
    fn spawn_arg_normalization_rejects_spawn_wrapper_payload() {
        let err = normalize_agent_spawn_args(&json!({
            "spawn": {"description": "Review one", "prompt": "p1"}
        }))
        .expect_err("wrapper payloads must be rejected");
        assert!(err.contains("wrapper"), "{err}");
        assert!(err.contains("top-level"), "{err}");
    }

    #[test]
    fn spawn_arg_normalization_rejects_redundant_background_policy_field() {
        for value in [true, false] {
            let err = normalize_agent_spawn_args(&json!({
            "action": "spawn",
            "description": "Review one",
            "prompt": "p1",
            "run_in_background": value,
            "_tool_call_id": "call-1"
            }))
            .expect_err("backgrounding is an explicit user control");
            assert!(err.contains("run_in_background"), "{err}");
            assert!(err.contains("Ctrl+B"), "{err}");
        }
    }

    #[tokio::test]
    async fn spawn_no_context_fails_explicitly() {
        let args = json!({
            "description": "Test",
            "prompt": "Test prompt"
        });
        let result = handle_agent_spawn_action(&args, None).await;
        assert!(
            result.contains("multi-agent runtime is not connected"),
            "{result}"
        );
        assert!(result.contains("tool_search"), "{result}");
        assert!(result.contains("\"status\":\"failed\""), "{result}");
    }

    struct CapturingModelExecutor {
        captured_model: Mutex<Option<String>>,
        captured_execution_metadata: Mutex<Option<Value>>,
        captured_max_turns: Mutex<Option<u32>>,
        spawn_count: Mutex<usize>,
    }

    impl CapturingModelExecutor {
        fn new() -> Self {
            Self {
                captured_model: Mutex::new(None),
                captured_execution_metadata: Mutex::new(None),
                captured_max_turns: Mutex::new(None),
                spawn_count: Mutex::new(0),
            }
        }

        fn take_captured_model(&self) -> Option<String> {
            self.captured_model.lock().unwrap().take()
        }

        fn take_captured_execution_metadata(&self) -> Option<Value> {
            self.captured_execution_metadata.lock().unwrap().take()
        }

        fn take_captured_max_turns(&self) -> Option<u32> {
            self.captured_max_turns.lock().unwrap().take()
        }

        fn spawn_count(&self) -> usize {
            *self.spawn_count.lock().unwrap()
        }
    }

    #[async_trait::async_trait]
    impl SpawnAgentExecutor for CapturingModelExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            *self.spawn_count.lock().unwrap() += 1;
            *self.captured_model.lock().unwrap() = config.model.clone();
            *self.captured_execution_metadata.lock().unwrap() = config.execution_metadata.clone();
            *self.captured_max_turns.lock().unwrap() = Some(config.max_turns);
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

    struct FixedOutputExecutor {
        output: String,
    }

    #[async_trait::async_trait]
    impl SpawnAgentExecutor for FixedOutputExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "completed".into(),
                finish_reason: "normal".into(),
                cancelled_by_user: None,
                output: Some(self.output.clone()),
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

    struct InterruptedSpawnExecutor;

    #[async_trait::async_trait]
    impl SpawnAgentExecutor for InterruptedSpawnExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "interrupted".into(),
                finish_reason: "budget_exhausted".into(),
                cancelled_by_user: None,
                output: Some("partial review".into()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 2,
                turns_completed: 0,
                permission_summary: None,
                permission_requests: 0,
                permission_requests_approved: 0,
                tools_blocked: 0,
            })
        }
    }

    struct ExecutionIncompleteSpawnExecutor;

    #[async_trait::async_trait]
    impl SpawnAgentExecutor for ExecutionIncompleteSpawnExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "interrupted".into(),
                finish_reason: "execution_incomplete".into(),
                cancelled_by_user: None,
                output: Some("The bound transport was unavailable.".into()),
                error: None,
                prompt_tokens: 10,
                completion_tokens: 5,
                tool_calls: 2,
                turns_completed: 0,
                permission_summary: None,
                permission_requests: 0,
                permission_requests_approved: 0,
                tools_blocked: 0,
            })
        }
    }

    struct FailedSpawnExecutor;

    #[async_trait::async_trait]
    impl SpawnAgentExecutor for FailedSpawnExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "failed".into(),
                finish_reason: "error".into(),
                cancelled_by_user: None,
                output: None,
                error: Some("child failed".into()),
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 1,
                turns_completed: 0,
                permission_summary: None,
                permission_requests: 0,
                permission_requests_approved: 0,
                tools_blocked: 0,
            })
        }
    }

    struct EmptyCompletionExecutor;

    #[async_trait::async_trait]
    impl SpawnAgentExecutor for EmptyCompletionExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "interrupted".into(),
                finish_reason: "empty_completion".into(),
                cancelled_by_user: None,
                output: Some(String::new()),
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

    struct ExecutorDroppedExecutor;

    #[async_trait::async_trait]
    impl SpawnAgentExecutor for ExecutorDroppedExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "failed".into(),
                finish_reason: "executor_dropped".into(),
                cancelled_by_user: None,
                output: None,
                error: Some("child completion payload was lost".into()),
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

    struct PendingExecutor;

    #[async_trait::async_trait]
    impl SpawnAgentExecutor for PendingExecutor {
        async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            std::future::pending::<Result<SpawnRunResult, String>>().await
        }
    }

    struct GatedFanoutExecutor {
        started_tx: tokio::sync::mpsc::UnboundedSender<String>,
        gates: Mutex<HashMap<String, tokio::sync::oneshot::Receiver<()>>>,
    }

    #[async_trait::async_trait]
    impl SpawnAgentExecutor for GatedFanoutExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            let description = config.description.clone();
            let _ = self.started_tx.send(description.clone());
            let gate = self
                .gates
                .lock()
                .unwrap()
                .remove(&description)
                .expect("test gate for child description");
            let _ = gate.await;
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "completed".into(),
                finish_reason: "normal".into(),
                cancelled_by_user: None,
                output: Some(format!("evidence from {description}")),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
                turns_completed: 1,
                permission_summary: None,
                permission_requests: 0,
                permission_requests_approved: 0,
                tools_blocked: 0,
            })
        }
    }

    fn test_spawner(executor: Arc<dyn SpawnAgentExecutor>) -> Arc<DynamicAgentSpawner> {
        let transport = Arc::new(astra_messaging::InProcessTransport::new());
        let tracker = Arc::new(DelegationTracker::new());
        let router = Arc::new(astra_messaging::AgentMailboxRouter::new(transport, tracker));
        Arc::new(DynamicAgentSpawner::new(router).with_executor(executor))
    }

    fn test_spawner_without_executor() -> Arc<DynamicAgentSpawner> {
        let transport = Arc::new(astra_messaging::InProcessTransport::new());
        let tracker = Arc::new(DelegationTracker::new());
        let router = Arc::new(astra_messaging::AgentMailboxRouter::new(transport, tracker));
        Arc::new(DynamicAgentSpawner::new(router))
    }

    fn test_spawn_context(
        spawner: Arc<DynamicAgentSpawner>,
        current_model: Option<&str>,
    ) -> AgentToolContext {
        AgentToolContext {
            run_id: "run-parent".into(),
            agent_id: "root-agent".into(),
            delegation_chain: Vec::new(),
            current_model: current_model.map(str::to_string),
            recursion_depth: 0,
            is_fork_child: false,
            working_dir: PathBuf::from("."),
            spawner,
            inherited_permissions: InheritedPermissions::auto_approve(),
            active_skills: Vec::new(),
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            execution_metadata: None,
            transcript_location: AgentTranscriptLocation::LocalJournal,
        }
    }

    async fn collect_spawn_receipt(receipt: &str, ctx: &AgentToolContext) -> Value {
        let value: Value = serde_json::from_str(receipt).expect("spawn result must be JSON");
        if value["status"] != "launched" {
            return value;
        }
        let agent_id = value["agent_id"]
            .as_str()
            .expect("background handoff must carry agent_id");
        let result =
            handle_agent_get_result_action(&json!({"agent_id": agent_id}), Some(ctx)).await;
        serde_json::from_str(&result).expect("collected agent result must be JSON")
    }

    async fn collect_fanout_start(start: &str, ctx: &AgentToolContext) -> Value {
        let value: Value = serde_json::from_str(start).expect("fanout result must be JSON");
        if value["status"] != "started" {
            return value;
        }
        let group_id = value["group_id"]
            .as_str()
            .expect("background fanout handoff must carry group_id");
        let result = handle_agent_fanout_tool(
            &json!({"action": "get_results", "group_id": group_id}),
            Some(ctx),
        )
        .await;
        serde_json::from_str(&result).expect("collected fanout result must be JSON")
    }

    #[tokio::test]
    async fn handle_spawn_agent_tool_inherits_parent_model_when_omitted() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor.clone());
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let args = json!({
            "description": "Code quality review",
            "prompt": "Review the latest commit",
            "agent_type": "general-purpose"
        });

        let result = handle_agent_spawn_action(&args, Some(&ctx)).await;

        assert_eq!(
            serde_json::from_str::<Value>(&result).unwrap()["status"],
            "completed"
        );
        let completed = collect_spawn_receipt(&result, &ctx).await;
        assert_eq!(completed["status"], "completed", "{completed}");
        assert_eq!(
            executor.take_captured_model().as_deref(),
            Some("MiniMax-M2.7")
        );
    }

    #[tokio::test]
    async fn spawn_receipt_declares_the_context_transcript_location() {
        let spawner = test_spawner(Arc::new(CapturingModelExecutor::new()));
        let mut ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        ctx.transcript_location = AgentTranscriptLocation::DurableServer;

        let output = handle_agent_spawn_action(
            &json!({
                "description": "Review the latest commit",
                "prompt": "Review the latest commit",
                "agent_type": "general-purpose"
            }),
            Some(&ctx),
        )
        .await;
        let receipt: Value = serde_json::from_str(&output).expect("spawn receipt is JSON");

        assert_eq!(receipt["transcript_location"], "durable_server");
        assert!(receipt["run_id"].as_str().is_some_and(|id| !id.is_empty()));
    }

    #[tokio::test]
    async fn handle_spawn_agent_tool_preserves_explicit_turn_budget() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor.clone());
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let args = json!({
            "description": "Investigate failures",
            "prompt": "Inspect the failing fanout run and report the root cause.",
            "agent_type": "general-purpose",
            "max_turns": 10,
            "complexity": "light"
        });

        let result = handle_agent_spawn_action(&args, Some(&ctx)).await;

        let completed = collect_spawn_receipt(&result, &ctx).await;
        assert_eq!(completed["status"], "completed", "{completed}");
        assert_eq!(
            executor.take_captured_max_turns(),
            Some(10),
            "the child loop must preserve an explicit caller-selected ceiling"
        );
    }

    #[tokio::test]
    async fn handle_spawn_agent_tool_inherits_execution_metadata() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor.clone());
        let mut ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        ctx.execution_metadata = Some(json!({
            "workspace": {
                "kind": "edge_workspace",
                "display_name": "MacBook Pro",
                "cwd": "/Users/xupeng/github/astra",
                "authority": "read_write"
            },
            "executor": {
                "kind": "edge_agent",
                "executor_id": "edge-macbook-1",
                "display_name": "MacBook Pro",
                "transport": "edge_ws",
                "status": "online"
            },
            "transport": "edge_ws"
        }));
        let args = json!({
            "description": "Code quality review",
            "prompt": "Review the latest commit",
            "agent_type": "general-purpose"
        });

        let result = handle_agent_spawn_action(&args, Some(&ctx)).await;

        let completed = collect_spawn_receipt(&result, &ctx).await;
        assert_eq!(completed["status"], "completed", "{completed}");
        let metadata = executor
            .take_captured_execution_metadata()
            .expect("execution metadata");
        assert_eq!(metadata["workspace"]["kind"], "edge_workspace");
        assert_eq!(metadata["executor"]["kind"], "edge_agent");
        assert_eq!(metadata["transport"], "edge_ws");
    }

    #[tokio::test]
    async fn handle_spawn_agent_tool_rejects_stray_agent_id() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor.clone());
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let args = json!({
            "description": "Security review",
            "prompt": "Review the auth changes",
            "agent_type": "general-purpose",
            "agent_id": "sec-review"
        });

        let result = handle_agent_spawn_action(&args, Some(&ctx)).await;

        assert!(result.contains("\"status\":\"failed\""), "{result}");
        assert!(result.contains("unsupported `agent_id` field"), "{result}");
        assert!(
            executor.take_captured_model().is_none(),
            "invalid spawn input must be rejected before launching an agent"
        );
    }

    #[tokio::test]
    async fn handle_spawn_agent_tool_rejects_invalid_fanout_slot_before_spawning() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor.clone());
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let args = json!({
            "description": "Storage review",
            "prompt": "Review storage layer",
            "agent_type": "general-purpose",
            "fanout_group_id": "review-1",
            "fanout_target_count": 3,
            "fanout_slot_index": 3
        });

        let result = handle_agent_spawn_action(&args, Some(&ctx)).await;

        assert!(result.contains("\"status\":\"failed\""), "{result}");
        assert!(result.contains("outside target_count"), "{result}");
        assert_eq!(
            executor.take_captured_model(),
            None,
            "invalid fanout metadata must fail before child execution"
        );
    }

    #[tokio::test]
    async fn agent_fanout_start_creates_fixed_group_slots() {
        let spawner = test_spawner(Arc::new(CapturingModelExecutor::new()));
        let ctx = test_spawn_context(spawner.clone(), Some("MiniMax-M2.7"));
        let result = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "group_id": "review-atomic",
                "title": "review fanout",
                "target_count": 2,
                "slots": [
                    {
                        "id": "storage",
                        "description": "Review storage",
                        "prompt": "Review storage changes and report correctness bugs.",
                        "agent_type": "code-review"
                    },
                    {
                        "id": "ui",
                        "description": "Review UI",
                        "prompt": "Review UI changes and report state bugs.",
                        "agent_type": "code-review"
                    }
                ]
            }),
            Some(&ctx),
        )
        .await;
        let value = collect_fanout_start(&result, &ctx).await;

        assert_eq!(value["status"], "completed");
        assert_eq!(value["group_id"], "review-atomic");
        assert_eq!(value["title"], "review fanout");
        assert_eq!(value["target_count"], 2);
        assert_eq!(value["results"].as_array().unwrap().len(), 2);
        assert_eq!(value["results"][0]["id"], "storage");
        assert_eq!(value["results"][1]["id"], "ui");

        let groups = spawner.list_fanout_groups().await;
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].group_id, "review-atomic");
        assert_eq!(groups[0].title, "review fanout");
        assert_eq!(groups[0].target_count, 2);
        assert_eq!(groups[0].parent_run_id.as_deref(), Some("run-parent"));
        assert_eq!(groups[0].slots[0].slot_id.as_deref(), Some("storage"));
        assert_eq!(groups[0].slots[1].slot_id.as_deref(), Some("ui"));
    }

    #[tokio::test]
    async fn completed_fanout_blocks_same_turn_direct_spawn_but_not_next_turn() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor.clone());
        let ctx = test_spawn_context(spawner.clone(), Some("MiniMax-M2.7"));

        let result = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "group_id": "analysis-atomic",
                "title": "analysis fanout",
                "target_count": 3,
                "slots": [
                    {"id": "storage", "description": "Inspect storage", "prompt": "Inspect storage changes."},
                    {"id": "runtime", "description": "Inspect runtime", "prompt": "Inspect runtime changes."},
                    {"id": "tests", "description": "Inspect tests", "prompt": "Inspect test coverage."}
                ]
            }),
            Some(&ctx),
        )
        .await;
        let value = collect_fanout_start(&result, &ctx).await;
        assert_eq!(value["status"], "completed");
        assert_eq!(value["target_count"], 3);
        assert_eq!(value["completed"], 3);
        assert_eq!(value["results"].as_array().unwrap().len(), 3);
        assert!(
            value["instruction"]
                .as_str()
                .is_some_and(|text| text.contains("target_count is complete")
                    && text.contains("Do not call agent(action='spawn')")),
            "{value}"
        );
        let _ = executor.take_captured_model();

        let blocked = handle_agent_spawn_action(
            &json!({
                "description": "Extra analysis",
                "prompt": "Run one more analysis.",
                "agent_type": "general-purpose"
            }),
            Some(&ctx),
        )
        .await;
        let blocked_value: Value = serde_json::from_str(&blocked).unwrap();
        assert_eq!(blocked_value["status"], "failed");
        let error = blocked_value["error"].as_str().unwrap_or_default();
        assert!(error.contains("already used agent_fanout"), "{error}");
        assert!(error.contains("target_count 3"), "{error}");
        assert!(error.contains("get_results"), "{error}");
        assert_eq!(
            executor.take_captured_model(),
            None,
            "blocked same-turn direct spawn must not launch a child"
        );

        let mut next_ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        next_ctx.run_id = "run-next-parent".to_string();
        let allowed = handle_agent_spawn_action(
            &json!({
                "description": "Fresh analysis",
                "prompt": "Analyze in the next user turn.",
                "agent_type": "general-purpose"
            }),
            Some(&next_ctx),
        )
        .await;
        let allowed_value: Value = serde_json::from_str(&allowed).unwrap();
        assert_eq!(allowed_value["status"], "completed");
        let completed = collect_spawn_receipt(&allowed, &next_ctx).await;
        assert_eq!(completed["status"], "completed");
        assert_eq!(
            executor.take_captured_model().as_deref(),
            Some("MiniMax-M2.7")
        );
    }

    #[tokio::test]
    async fn parent_run_fanout_start_is_idempotent_and_rejects_a_second_group() {
        let spawner = test_spawner(Arc::new(CapturingModelExecutor::new()));
        let ctx = test_spawn_context(spawner.clone(), Some("MiniMax-M2.7"));
        let first_args = json!({
            "action": "start",
            "_tool_call_id": "call-first",
            "group_id": "review-first",
            "target_count": 1,
            "slots": [{"id": "correctness", "description": "Review correctness", "prompt": "Review correctness."}]
        });
        let first = handle_agent_fanout_tool(&first_args, Some(&ctx)).await;
        assert_eq!(
            serde_json::from_str::<Value>(&first).unwrap()["status"],
            "completed"
        );

        let replay = handle_agent_fanout_tool(&first_args, Some(&ctx)).await;
        let replay: Value = serde_json::from_str(&replay).unwrap();
        assert_eq!(replay["group_id"], "review-first");
        assert_eq!(spawner.list_fanout_groups().await.len(), 1);

        let second = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "_tool_call_id": "call-second",
                "group_id": "review-second",
                "target_count": 1,
                "slots": [{"id": "security", "description": "Review security", "prompt": "Review security."}]
            }),
            Some(&ctx),
        )
        .await;
        let second: Value = serde_json::from_str(&second).unwrap();
        assert_eq!(second["status"], "failed");
        assert!(
            second["error"]
                .as_str()
                .is_some_and(|error| error.contains("may start only one fanout group")),
            "{second}"
        );
        assert_eq!(spawner.list_fanout_groups().await.len(), 1);
    }

    #[tokio::test]
    async fn fanout_executor_unavailable_fails_before_declaring_group_or_blocking_spawn() {
        let spawner = test_spawner_without_executor();
        let ctx = test_spawn_context(spawner.clone(), Some("MiniMax-M2.7"));

        let result = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "group_id": "startup-unavailable",
                "target_count": 1,
                "slots": [
                    {"id": "startup", "description": "Inspect startup", "prompt": "Inspect startup path."}
                ]
            }),
            Some(&ctx),
        )
        .await;
        let value: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(value["status"], "failed");
        assert_eq!(
            value["error_kind"].as_str(),
            Some(astra_core::ErrorKind::ToolBinding.as_str())
        );
        assert!(
            value["error"]
                .as_str()
                .is_some_and(|text| text.contains("multi-agent runtime is not connected")
                    && text.contains("tool_search")),
            "{value}"
        );

        let groups = spawner.list_fanout_groups().await;
        assert!(
            groups.is_empty(),
            "host capability failures must not leave empty fanout groups: {groups:?}"
        );

        let direct = handle_agent_spawn_action(
            &json!({
                "description": "Replacement",
                "prompt": "Try to replace the failed slot.",
                "agent_type": "general-purpose"
            }),
            Some(&ctx),
        )
        .await;
        let direct_value: Value = serde_json::from_str(&direct).unwrap();
        assert_eq!(direct_value["status"], "failed");
        assert_eq!(
            direct_value["error_kind"].as_str(),
            Some(astra_core::ErrorKind::ToolBinding.as_str())
        );
        assert!(
            !direct_value["error"]
                .as_str()
                .is_some_and(|text| text.contains("already used agent_fanout")),
            "direct spawn should fail because the executor is unavailable, not because a ghost fanout group was registered: {direct_value}"
        );
    }

    #[tokio::test]
    async fn agent_fanout_start_waits_for_and_returns_canonical_results() {
        let spawner = test_spawner(Arc::new(CapturingModelExecutor::new()));
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let result = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "_tool_call_id": "call-structured",
                "group_id": "review-structured",
                "target_count": 1,
                "slots": [
                    {
                        "id": "storage",
                        "description": "Review storage",
                        "prompt": "Review storage changes"
                    }
                ]
            }),
            Some(&ctx),
        )
        .await;
        let value: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(value["status"], "completed");
        assert_eq!(value["group_id"], "review-structured");
        assert!(
            value["instruction"]
                .as_str()
                .is_some_and(|instruction| instruction.contains("target_count is complete")),
            "{value}"
        );
        assert_eq!(value["results"].as_array().unwrap().len(), 1);
        assert_eq!(value["results"][0]["id"], "storage");
        assert_eq!(value["results"][0]["result"]["status"], "completed");
        assert_eq!(value["completed"], 1);
    }

    #[tokio::test]
    async fn fanout_starts_children_concurrently_and_waits_for_the_whole_group() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_a, gate_a) = tokio::sync::oneshot::channel();
        let (release_b, gate_b) = tokio::sync::oneshot::channel();
        let executor = Arc::new(GatedFanoutExecutor {
            started_tx,
            gates: Mutex::new(HashMap::from([
                ("Review A".to_string(), gate_a),
                ("Review B".to_string(), gate_b),
            ])),
        });
        let spawner = test_spawner(executor);
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let start_ctx = ctx.clone();
        let start_task = tokio::spawn(async move {
            handle_agent_fanout_tool(
                &json!({
                    "action": "start",
                    "group_id": "review-gated",
                    "target_count": 2,
                    "slots": [
                        {"id": "a", "description": "Review A", "prompt": "Review A."},
                        {"id": "b", "description": "Review B", "prompt": "Review B."}
                    ]
                }),
                Some(&start_ctx),
            )
            .await
        });

        let first = tokio::time::timeout(Duration::from_secs(1), started_rx.recv())
            .await
            .expect("first child must start")
            .expect("started channel");
        let second = tokio::time::timeout(Duration::from_secs(1), started_rx.recv())
            .await
            .expect("second child must start without waiting for the first")
            .expect("started channel");
        assert_eq!(
            HashSet::from([first, second]),
            HashSet::from(["Review A".to_string(), "Review B".to_string(),])
        );
        assert!(
            !start_task.is_finished(),
            "fan-in cannot settle while both children are gated"
        );

        release_a.send(()).expect("release first child");
        tokio::task::yield_now().await;
        assert!(
            !start_task.is_finished(),
            "one child completion must not give the parent a model boundary"
        );

        release_b.send(()).expect("release second child");
        let result = tokio::time::timeout(Duration::from_secs(1), start_task)
            .await
            .expect("whole group must settle after the last child")
            .expect("fanout task must not panic");
        let value: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(value["status"], "completed");
        assert_eq!(value["completed"], 2);
        assert_eq!(value["results"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn agent_fanout_start_rejects_slot_count_mismatch_before_spawning() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor.clone());
        let ctx = test_spawn_context(spawner.clone(), Some("MiniMax-M2.7"));
        let result = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "group_id": "review-atomic",
                "target_count": 2,
                "slots": [
                    {"description": "Review one", "prompt": "Review one"}
                ]
            }),
            Some(&ctx),
        )
        .await;

        assert!(result.contains("\"status\":\"failed\""), "{result}");
        assert!(result.contains("requires exactly 2 slots"), "{result}");
        assert!(spawner.list_fanout_groups().await.is_empty());
        assert_eq!(executor.take_captured_model(), None);
    }

    #[tokio::test]
    async fn agent_fanout_start_rejects_unknown_top_level_fields_before_spawning() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor.clone());
        let ctx = test_spawn_context(spawner.clone(), Some("MiniMax-M2.7"));
        let result = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "group_id": "review-atomic",
                "target_count": 1,
                "agents": [],
                "slots": [
                    {"description": "Review one", "prompt": "Review one"}
                ]
            }),
            Some(&ctx),
        )
        .await;

        assert!(result.contains("\"status\":\"failed\""), "{result}");
        assert!(result.contains("unknown field"), "{result}");
        assert!(result.contains("agents"), "{result}");
        assert!(result.contains("agent_fanout.start"), "{result}");
        assert!(result.contains("Valid fields"), "{result}");
        assert!(result.contains("slots"), "{result}");
        assert!(spawner.list_fanout_groups().await.is_empty());
        assert_eq!(executor.take_captured_model(), None);
    }

    #[tokio::test]
    async fn agent_fanout_start_rejects_top_level_brief_with_canonical_shape() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor.clone());
        let ctx = test_spawn_context(spawner.clone(), Some("MiniMax-M2.7"));
        let result = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "brief": "Review the auth stack",
                "target_count": 1,
                "slots": [
                    {"description": "Review auth", "prompt": "Review auth stack"}
                ]
            }),
            Some(&ctx),
        )
        .await;

        assert!(result.contains("\"status\":\"failed\""), "{result}");
        assert!(result.contains("unknown field `brief`"), "{result}");
        assert!(result.contains("slots[i].prompt"), "{result}");
        assert!(result.contains("there is no top-level brief"), "{result}");
        assert!(spawner.list_fanout_groups().await.is_empty());
        assert_eq!(executor.take_captured_model(), None);
    }

    #[tokio::test]
    async fn agent_fanout_start_rejects_unknown_slot_fields_before_spawning() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor.clone());
        let ctx = test_spawn_context(spawner.clone(), Some("MiniMax-M2.7"));
        let result = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "group_id": "review-atomic",
                "target_count": 1,
                "slots": [
                    {
                        "description": "Review one",
                        "prompt": "Review one",
                        "task": "legacy task field"
                    }
                ]
            }),
            Some(&ctx),
        )
        .await;

        assert!(result.contains("\"status\":\"failed\""), "{result}");
        assert!(result.contains("unknown field"), "{result}");
        assert!(result.contains("task"), "{result}");
        assert!(spawner.list_fanout_groups().await.is_empty());
        assert_eq!(executor.take_captured_model(), None);
    }

    #[tokio::test]
    async fn agent_fanout_start_rejects_empty_slot_id_before_spawning() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor.clone());
        let ctx = test_spawn_context(spawner.clone(), Some("MiniMax-M2.7"));
        let result = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "group_id": "review-atomic",
                "target_count": 1,
                "slots": [
                    {
                        "id": "   ",
                        "description": "Review one",
                        "prompt": "Review one"
                    }
                ]
            }),
            Some(&ctx),
        )
        .await;

        assert!(result.contains("\"status\":\"failed\""), "{result}");
        assert!(result.contains("id must be non-empty"), "{result}");
        assert!(spawner.list_fanout_groups().await.is_empty());
        assert_eq!(executor.take_captured_model(), None);
    }

    #[tokio::test]
    async fn agent_fanout_start_rejects_duplicate_slot_id_before_spawning() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor.clone());
        let ctx = test_spawn_context(spawner.clone(), Some("MiniMax-M2.7"));
        let result = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "group_id": "review-atomic",
                "target_count": 2,
                "slots": [
                    {"id": "storage", "description": "Review one", "prompt": "Review one"},
                    {"id": " storage ", "description": "Review two", "prompt": "Review two"}
                ]
            }),
            Some(&ctx),
        )
        .await;

        assert!(result.contains("\"status\":\"failed\""), "{result}");
        assert!(result.contains("id"), "{result}");
        assert!(result.contains("duplicated"), "{result}");
        assert!(spawner.list_fanout_groups().await.is_empty());
        assert_eq!(executor.take_captured_model(), None);
    }

    #[test]
    fn fanout_slot_spawn_args_carry_group_title_for_ui_projection() {
        let input = AgentFanoutStartInput {
            _action: Some("start".into()),
            _tool_call_id: None,
            group_id: Some("review-1".into()),
            title: Some("review fanout".into()),
            target_count: 3,
            slots: Vec::new(),
            defaults: None,
        };
        let slot = AgentFanoutStartSlot {
            slot_id: Some("storage".into()),
            description: "Review storage".into(),
            prompt: "Review storage layer".into(),
            agent_type: None,
            model: None,
            max_turns: None,
            max_output_tokens: None,
            complexity: None,
            isolated: None,
            allowed_tools: None,
        };

        let args = fanout_slot_spawn_args(&input, slot, "review-1", "review fanout", 3, 1, None);

        assert_eq!(args["fanout_group_id"], "review-1");
        assert_eq!(args["fanout_group_title"], "review fanout");
        assert_eq!(args["fanout_target_count"], 3);
        assert_eq!(args["fanout_slot_index"], 1);
        assert_eq!(args["fanout_slot_id"], "storage");
        assert!(args.get("name").is_none());
    }

    #[test]
    fn fanout_slot_spawn_args_preserve_explicit_deep_review_budget() {
        let input = AgentFanoutStartInput {
            _action: Some("start".into()),
            _tool_call_id: None,
            group_id: Some("review-1".into()),
            title: Some("review fanout".into()),
            target_count: 4,
            slots: Vec::new(),
            defaults: Some(AgentFanoutDefaults {
                agent_type: Some("code-review".into()),
                max_turns: Some(15),
                complexity: Some("deep".into()),
                ..Default::default()
            }),
        };
        let slot = AgentFanoutStartSlot {
            slot_id: Some("correctness".into()),
            description: "Review correctness".into(),
            prompt: "Review correctness deeply".into(),
            agent_type: None,
            model: None,
            max_turns: None,
            max_output_tokens: None,
            complexity: None,
            isolated: None,
            allowed_tools: None,
        };

        let args = fanout_slot_spawn_args(&input, slot, "review-1", "review fanout", 4, 1, None);

        assert_eq!(args["agent_type"], "code-review");
        assert_eq!(args["complexity"], "deep");
        assert_eq!(args["max_turns"], 15);
    }

    #[test]
    fn fanout_slot_spawn_args_preserve_explicit_general_purpose_budget() {
        let input = AgentFanoutStartInput {
            _action: Some("start".into()),
            _tool_call_id: None,
            group_id: Some("investigate-1".into()),
            title: Some("investigation fanout".into()),
            target_count: 2,
            slots: Vec::new(),
            defaults: Some(AgentFanoutDefaults {
                agent_type: Some("general-purpose".into()),
                max_turns: Some(10),
                complexity: Some("light".into()),
                ..Default::default()
            }),
        };
        let slot = AgentFanoutStartSlot {
            slot_id: Some("runtime".into()),
            description: "Investigate runtime".into(),
            prompt: "Investigate runtime failures".into(),
            agent_type: None,
            model: None,
            max_turns: None,
            max_output_tokens: None,
            complexity: None,
            isolated: None,
            allowed_tools: None,
        };

        let args = fanout_slot_spawn_args(
            &input,
            slot,
            "investigate-1",
            "investigation fanout",
            2,
            0,
            None,
        );

        assert_eq!(args["agent_type"], "general-purpose");
        assert_eq!(args["complexity"], "light");
        assert_eq!(args["max_turns"], 10);
    }

    #[tokio::test]
    async fn agent_fanout_get_results_collects_all_slots() {
        let spawner = test_spawner(Arc::new(CapturingModelExecutor::new()));
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let start = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "group_id": "review-atomic",
                "target_count": 1,
                "slots": [
                    {"description": "Review storage", "prompt": "Review storage changes"}
                ]
            }),
            Some(&ctx),
        )
        .await;
        let start_value: Value = serde_json::from_str(&start).unwrap();
        assert_eq!(start_value["status"], "completed");

        let result = handle_agent_fanout_tool(
            &json!({
                "action": "get_results",
                "_tool_call_id": "call-get-results",
                "group_id": "review-atomic"
            }),
            Some(&ctx),
        )
        .await;
        let value: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(value["status"], "completed");
        assert_eq!(value["results"].as_array().unwrap().len(), 1);
        assert_eq!(value["results"][0]["result"]["status"], "completed");
        assert_eq!(value["completed"], 1);
        assert_eq!(value["transcript_location"], "local_journal");
        assert!(
            value["fanout"]["slots"][0]["run_id"]
                .as_str()
                .is_some_and(|run_id| !run_id.is_empty()),
            "recovered fanout results must retain every child transcript identity: {result}"
        );
        assert!(
            value["delivery_contract"]
                .as_str()
                .is_some_and(|text| text.contains("results[].next_call")),
            "{result}"
        );
        assert!(
            value["delivery_contract"].as_str().is_some_and(|text| {
                text.contains("slot_index=N")
                    && text.contains("Do not search for or copy physical filesystem paths")
            }),
            "{result}"
        );
        assert_eq!(
            value["result_read"]["max_bytes"],
            FANOUT_RESULT_DEFAULT_MAX_BYTES
        );
        assert_eq!(value["fanout"]["slots"].as_array().unwrap().len(), 1);
        assert!(
            result.contains('\n'),
            "fanout aggregate results must be readable when persisted: {result}"
        );
    }

    #[tokio::test]
    async fn agent_fanout_get_results_returns_bounded_slot_windows_for_large_outputs() {
        let output = format!("{}{}", "A".repeat(9000), "B".repeat(9000));
        let spawner = test_spawner(Arc::new(FixedOutputExecutor { output }));
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let start = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "group_id": "review-large",
                "target_count": 1,
                "slots": [
                    {"id": "large", "description": "Review large", "prompt": "Return long output"}
                ]
            }),
            Some(&ctx),
        )
        .await;
        let start_value: Value = serde_json::from_str(&start).unwrap();
        assert_eq!(start_value["status"], "completed");

        let result = handle_agent_fanout_tool(
            &json!({
                "action": "get_results",
                "group_id": "review-large"
            }),
            Some(&ctx),
        )
        .await;
        let value: Value = serde_json::from_str(&result).unwrap();
        let slot = &value["results"][0];
        let preview = slot["result"]["result"].as_str().unwrap();

        assert_eq!(value["status"], "completed");
        assert_eq!(slot["result_start_offset"], 0);
        assert_eq!(slot["result_end_offset"], FANOUT_RESULT_DEFAULT_MAX_BYTES);
        assert_eq!(slot["result_bytes"], 18_000);
        assert_eq!(slot["result_truncated"], true);
        assert_eq!(preview.len(), FANOUT_RESULT_DEFAULT_MAX_BYTES);
        assert!(
            slot["next_call"]
                .as_str()
                .is_some_and(|call| call.contains("slot_index=0")
                    && call.contains("offset=8192")
                    && call.contains("max_bytes=8192")),
            "{result}"
        );
        assert!(
            !result.contains("artifact://session/tool-result"),
            "fanout recovery must point at the owning tool window API, not internal artifacts: {result}"
        );
    }

    #[tokio::test]
    async fn agent_fanout_get_results_reads_requested_slot_window() {
        let output = format!("{}{}", "A".repeat(9000), "B".repeat(9000));
        let spawner = test_spawner(Arc::new(FixedOutputExecutor { output }));
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let start = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "group_id": "review-window",
                "target_count": 1,
                "slots": [
                    {"id": "large", "description": "Review large", "prompt": "Return long output"}
                ]
            }),
            Some(&ctx),
        )
        .await;
        let start_value: Value = serde_json::from_str(&start).unwrap();
        assert_eq!(start_value["status"], "completed");

        let result = handle_agent_fanout_tool(
            &json!({
                "action": "get_results",
                "group_id": "review-window",
                "slot_index": 0,
                "offset": 8192,
                "max_bytes": 4096
            }),
            Some(&ctx),
        )
        .await;
        let value: Value = serde_json::from_str(&result).unwrap();
        let slot = &value["results"][0];
        let chunk = slot["result"]["result"].as_str().unwrap();

        assert_eq!(value["results"].as_array().unwrap().len(), 1);
        assert_eq!(value["result_read"]["slot_index"], 0);
        assert_eq!(value["result_read"]["offset"], 8192);
        assert_eq!(value["result_read"]["max_bytes"], 4096);
        assert_eq!(slot["result_start_offset"], 8192);
        assert_eq!(slot["result_end_offset"], 12288);
        assert_eq!(slot["result_bytes"], 18_000);
        assert_eq!(chunk.len(), 4096);
        assert!(chunk.starts_with('A'), "{chunk}");
        assert!(chunk.contains('B'), "{chunk}");
        assert!(
            slot["next_call"].as_str().is_some_and(
                |call| call.contains("offset=12288") && call.contains("max_bytes=4096")
            ),
            "{result}"
        );
        assert!(
            slot["result"].get("fanout").is_none(),
            "group accounting must appear once in the outer envelope, not once per slot: {result}"
        );
    }

    #[tokio::test]
    async fn agent_fanout_terminal_manifest_stays_directly_consumable() {
        let output = "R".repeat(15_000);
        let spawner = test_spawner(Arc::new(FixedOutputExecutor { output }));
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let start = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "group_id": "review-consumable",
                "target_count": 3,
                "slots": [
                    {"id": "one", "description": "Review one", "prompt": "Review one"},
                    {"id": "two", "description": "Review two", "prompt": "Review two"},
                    {"id": "three", "description": "Review three", "prompt": "Review three"}
                ]
            }),
            Some(&ctx),
        )
        .await;
        let started: Value = serde_json::from_str(&start).unwrap();
        assert_eq!(started["status"], "completed");

        let rendered = handle_agent_fanout_tool(
            &json!({
                "action": "get_results",
                "group_id": "review-consumable"
            }),
            Some(&ctx),
        )
        .await;
        let value: Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(value["results"].as_array().unwrap().len(), 3);
        assert_eq!(
            value[WORK_UNIT_OBSERVATION_FIELD]["wake_policy"],
            "on_terminal"
        );
        assert!(
            rendered.len() < 40_000,
            "a routine three-agent terminal manifest must stay below tool artifact handoff size; got {} bytes",
            rendered.len()
        );
        assert!(value["results"].as_array().unwrap().iter().all(|slot| {
            slot["result"].get("fanout").is_none()
                && slot["result_truncated"] == true
                && slot["next_call"].is_string()
        }));
    }

    #[tokio::test]
    async fn recover_agent_fanout_start_uses_existing_group_without_respawn() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor.clone());
        let ctx = test_spawn_context(spawner.clone(), Some("MiniMax-M2.7"));
        let start_args = json!({
            "action": "start",
            "_tool_call_id": "call-start",
            "group_id": "review-recover",
            "target_count": 2,
            "slots": [
                {"id": "first", "description": "Review storage", "prompt": "Review storage changes"},
                {"id": "second", "description": "Review auth", "prompt": "Review auth changes"}
            ]
        });
        let start = handle_agent_fanout_tool(&start_args, Some(&ctx)).await;
        let completed = collect_fanout_start(&start, &ctx).await;
        assert_eq!(completed["status"], "completed");
        assert_eq!(executor.spawn_count(), 2);

        let recovered =
            recover_agent_fanout_tool_result(&start_args, Some("call-start"), Some(&ctx)).await;
        let value: Value = serde_json::from_str(&recovered).unwrap();

        assert_eq!(value["status"], "completed");
        assert_eq!(value["group_id"], "review-recover");
        assert_eq!(value["completed"], 2);
        assert_eq!(value["results"].as_array().unwrap().len(), 2);
        assert!(
            value["instruction"]
                .as_str()
                .is_some_and(|text| text.contains("Do not call agent(action='spawn')")),
            "{recovered}"
        );
        assert_eq!(
            executor.spawn_count(),
            2,
            "recovering a missing edge row must not duplicate child agents"
        );
        assert_eq!(spawner.list_fanout_groups().await.len(), 1);
    }

    #[tokio::test]
    async fn recover_agent_fanout_start_without_registered_group_fails_without_respawn() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor.clone());
        let ctx = test_spawn_context(spawner.clone(), Some("MiniMax-M2.7"));
        let recovered = recover_agent_fanout_tool_result(
            &json!({
                "action": "start",
                "target_count": 1,
                "slots": [
                    {"description": "Review storage", "prompt": "Review changes"}
                ]
            }),
            Some("call-missing-start"),
            Some(&ctx),
        )
        .await;
        let value: Value = serde_json::from_str(&recovered).unwrap();

        assert_eq!(value["status"], "failed");
        assert!(
            value["error"]
                .as_str()
                .is_some_and(|text| text.contains("no registered fanout group")),
            "{recovered}"
        );
        assert_eq!(
            executor.spawn_count(),
            0,
            "recovery must not replay agent_fanout.start when no registry state exists"
        );
        assert!(spawner.list_fanout_groups().await.is_empty());
    }

    #[tokio::test]
    async fn agent_fanout_start_refuses_second_group_for_same_parent() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor.clone());
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let first = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "group_id": "review-one",
                "target_count": 1,
                "slots": [
                    {"description": "Review first", "prompt": "Review changes"}
                ]
            }),
            Some(&ctx),
        )
        .await;
        let completed = collect_fanout_start(&first, &ctx).await;
        assert_eq!(completed["status"], "completed");

        let second = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "group_id": "review-two",
                "target_count": 1,
                "slots": [
                    {"description": "Review second", "prompt": "Review changes"}
                ]
            }),
            Some(&ctx),
        )
        .await;
        let second_value: Value = serde_json::from_str(&second).unwrap();

        assert_eq!(second_value["status"], "failed");
        assert!(
            second_value["error"]
                .as_str()
                .is_some_and(|text| text.contains("a parent run may start only one fanout group")),
            "{second}"
        );
        assert_eq!(
            executor.spawn_count(),
            1,
            "a rejected second group must not spawn any replacement child"
        );
    }

    #[tokio::test]
    async fn recover_agent_fanout_start_refuses_unknown_explicit_group_id() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor.clone());
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let existing = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "group_id": "existing-review",
                "target_count": 1,
                "slots": [
                    {"description": "Review existing", "prompt": "Review existing changes"}
                ]
            }),
            Some(&ctx),
        )
        .await;
        let existing_value: Value = serde_json::from_str(&existing).unwrap();
        assert_eq!(existing_value["group_id"], "existing-review");
        let completed = collect_fanout_start(&existing, &ctx).await;
        assert_eq!(completed["status"], "completed");
        assert_eq!(executor.spawn_count(), 1);

        let recovered = recover_agent_fanout_tool_result(
            &json!({
                "action": "start",
                "group_id": "new-review",
                "target_count": 1,
                "slots": [
                    {"description": "Review new", "prompt": "Review new changes"}
                ]
            }),
            None,
            Some(&ctx),
        )
        .await;
        let value: Value = serde_json::from_str(&recovered).unwrap();

        assert_eq!(value["status"], "failed");
        assert!(
            value["error"]
                .as_str()
                .is_some_and(|text| text.contains("requested group_id 'new-review'")),
            "{recovered}"
        );
        assert_eq!(
            executor.spawn_count(),
            1,
            "recovery must not start a new explicit group when no matching registry state exists"
        );
    }

    #[tokio::test]
    async fn recover_agent_fanout_stop_slot_refuses_side_effect_replay() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor.clone());
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let start = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "group_id": "review-stop-recovery",
                "target_count": 1,
                "slots": [
                    {"description": "Review existing", "prompt": "Review existing changes"}
                ]
            }),
            Some(&ctx),
        )
        .await;
        let completed = collect_fanout_start(&start, &ctx).await;
        assert_eq!(completed["status"], "completed");
        assert_eq!(executor.spawn_count(), 1);

        let recovered = recover_agent_fanout_tool_result(
            &json!({
                "action": "stop_slot",
                "group_id": "review-stop-recovery",
                "slot_index": 0
            }),
            Some("call-stop-slot"),
            Some(&ctx),
        )
        .await;
        let value: Value = serde_json::from_str(&recovered).unwrap();

        assert_eq!(value["status"], "failed");
        assert!(
            value["error"]
                .as_str()
                .is_some_and(|text| text.contains("stop_slot has side effects")),
            "{recovered}"
        );
    }

    #[tokio::test]
    async fn agent_fanout_get_results_preserves_spawn_rejected_slot_status() {
        let spawner = test_spawner(Arc::new(CapturingModelExecutor::new()));
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let start = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "group_id": "review-atomic",
                "target_count": 2,
                "slots": [
                    {
                        "description": "Review storage",
                        "prompt": "Review storage changes",
                        "agent_type": "not-a-real-agent-type"
                    },
                    {
                        "description": "Review runtime",
                        "prompt": "Review runtime changes"
                    }
                ]
            }),
            Some(&ctx),
        )
        .await;
        let start_value: Value = serde_json::from_str(&start).unwrap();
        assert_eq!(start_value["status"], "completed_with_issues");
        assert_eq!(start_value["spawn_rejected"], 1);

        let result = handle_agent_fanout_tool(
            &json!({"action": "get_results", "group_id": "review-atomic"}),
            Some(&ctx),
        )
        .await;
        let value: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(value["status"], "completed_with_issues");
        assert_eq!(value["results"].as_array().unwrap().len(), 2);
        assert_eq!(value["results"][0]["slot_index"], 0);
        assert_eq!(value["results"][0]["status"], "spawn_rejected");
        assert!(
            value["results"][0]["error"]
                .as_str()
                .is_some_and(|error| error.contains("unknown agent type")),
            "rejected slot result should preserve the rejection reason: {value}"
        );
        assert_eq!(value["results"][1]["slot_index"], 1);
        assert_eq!(value["results"][1]["result"]["status"], "completed");
        assert_eq!(value["completed"], 1);
        assert_eq!(value["spawn_rejected"], 1);
    }

    #[tokio::test]
    async fn agent_fanout_get_results_reports_failed_child_as_failed_fanout() {
        let spawner = test_spawner(Arc::new(FailedSpawnExecutor));
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let start = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "group_id": "review-atomic",
                "target_count": 1,
                "slots": [
                    {"description": "Review storage", "prompt": "Review storage changes"}
                ]
            }),
            Some(&ctx),
        )
        .await;
        let start_value: Value = serde_json::from_str(&start).unwrap();
        assert_eq!(start_value["status"], "completed_with_issues");
        assert_eq!(start_value["failed"], 1);

        let result = handle_agent_fanout_tool(
            &json!({"action": "get_results", "group_id": "review-atomic"}),
            Some(&ctx),
        )
        .await;
        let value: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(value["status"], "completed_with_issues");
        assert_eq!(value["results"].as_array().unwrap().len(), 1);
        assert_eq!(value["results"][0]["slot_index"], 0);
        assert_eq!(value["results"][0]["result"]["status"], "failed");
        assert_eq!(value["failed"], 1);

        let blocked = handle_agent_spawn_action(
            &json!({
                "description": "Replacement",
                "prompt": "Try to replace the failed slot.",
                "agent_type": "general-purpose"
            }),
            Some(&ctx),
        )
        .await;
        let blocked_value: Value = serde_json::from_str(&blocked).unwrap();
        assert_eq!(blocked_value["status"], "failed");
        assert!(
            blocked_value["error"]
                .as_str()
                .is_some_and(|text| text.contains("already used agent_fanout")),
            "{blocked_value}"
        );
    }

    #[tokio::test]
    async fn agent_fanout_stop_slot_reports_rejected_slot_as_not_stoppable() {
        let spawner = test_spawner(Arc::new(CapturingModelExecutor::new()));
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let start = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "group_id": "review-atomic",
                "target_count": 1,
                "slots": [
                    {
                        "description": "Review storage",
                        "prompt": "Review storage changes",
                        "agent_type": "not-a-real-agent-type"
                    }
                ]
            }),
            Some(&ctx),
        )
        .await;
        let start_value: Value = serde_json::from_str(&start).unwrap();
        assert_eq!(start_value["status"], "completed_with_issues");
        assert_eq!(start_value["spawn_rejected"], 1);

        let result = handle_agent_fanout_tool(
            &json!({
                "action": "stop_slot",
                "group_id": "review-atomic",
                "slot_index": 0
            }),
            Some(&ctx),
        )
        .await;
        let value: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(value["status"], "not_stoppable");
        assert_eq!(value["reason"], "no_accepted_agent");
        assert_eq!(value["slot_index"], 0);
        assert_eq!(value["slot_status"], "spawn_rejected");
        assert!(
            value["terminal_reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("unknown agent type")),
            "stop_slot should preserve why the slot cannot be stopped: {value}"
        );
        assert_eq!(value["fanout"]["spawn_rejected"], 1);
        assert_eq!(value["fanout"]["slots"][0]["status"], "spawn_rejected");
    }

    #[tokio::test]
    async fn agent_fanout_stop_slot_reports_terminal_slot_as_not_stoppable() {
        let spawner = test_spawner(Arc::new(CapturingModelExecutor::new()));
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let start = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "group_id": "review-atomic",
                "target_count": 1,
                "slots": [
                    {"description": "Review storage", "prompt": "Review storage changes"}
                ]
            }),
            Some(&ctx),
        )
        .await;
        let completed = collect_fanout_start(&start, &ctx).await;
        assert_eq!(completed["status"], "completed");

        let result = handle_agent_fanout_tool(
            &json!({
                "action": "stop_slot",
                "group_id": "review-atomic",
                "slot_index": 0
            }),
            Some(&ctx),
        )
        .await;
        let value: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(value["status"], "not_stoppable");
        assert_eq!(value["reason"], "already_terminal");
        assert_eq!(value["slot_status"], "completed");
        assert_eq!(value["fanout"]["completed"], 1);
        assert_eq!(value["fanout"]["slots"][0]["status"], "completed");
        assert_eq!(value["fanout"]["slots"][0]["result_collected"], true);
    }

    #[tokio::test]
    async fn agent_fanout_stop_slot_then_get_results_reports_stopped_by_user() {
        let spawner = test_spawner(Arc::new(PendingExecutor));
        let ctx = test_spawn_context(spawner.clone(), Some("MiniMax-M2.7"));
        let start_ctx = ctx.clone();
        let start_task = tokio::spawn(async move {
            handle_agent_fanout_tool(
                &json!({
                    "action": "start",
                    "group_id": "review-atomic",
                    "target_count": 1,
                    "slots": [
                        {"description": "Review storage", "prompt": "Review storage changes"}
                    ]
                }),
                Some(&start_ctx),
            )
            .await
        });
        for _ in 0..100 {
            if spawner.list_all_agents().await.len() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        let running = spawner.list_fanout_groups().await;
        assert_eq!(running.len(), 1, "fanout must be visible while start waits");
        assert_eq!(running[0].parent_run_id.as_deref(), Some("run-parent"));
        assert!(
            running[0].slots[0]
                .run_id
                .as_deref()
                .is_some_and(|run_id| !run_id.is_empty())
        );

        let stop = handle_agent_fanout_tool(
            &json!({
                "action": "stop_slot",
                "_tool_call_id": "call-stop-slot",
                "group_id": "review-atomic",
                "slot_index": 0
            }),
            Some(&ctx),
        )
        .await;
        let stop_value: Value = serde_json::from_str(&stop).unwrap();
        assert_eq!(stop_value["status"], "stopped");
        assert_eq!(stop_value["slot_status"], "cancelled_by_user");
        assert_eq!(stop_value["fanout"]["cancelled_by_user"], 1);

        let start = tokio::time::timeout(Duration::from_secs(1), start_task)
            .await
            .expect("cancelling the slot must unblock foreground fan-in")
            .expect("fanout start task must not panic");
        let start_value: Value = serde_json::from_str(&start).unwrap();
        assert_eq!(start_value["status"], "completed_with_issues");
        assert_eq!(start_value["transcript_location"], "local_journal");

        let result = handle_agent_fanout_tool(
            &json!({"action": "get_results", "group_id": "review-atomic"}),
            Some(&ctx),
        )
        .await;
        let value: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(value["status"], "completed_with_issues");
        assert_eq!(value["cancelled_by_user"], 1);
    }

    #[test]
    fn fanout_get_results_status_names_non_successful_terminal_groups() {
        let mut spawn_rejected = AgentFanoutGroupProjection::new("review-1", "Review", 1);
        spawn_rejected.record_spawn_rejected(0, "quota").unwrap();
        assert_eq!(
            fanout_get_results_status_label(&spawn_rejected),
            "completed_with_issues"
        );

        let mut failed = AgentFanoutGroupProjection::new("review-2", "Review", 1);
        failed.record_spawn_accepted(0, "auth@aaa").unwrap();
        failed
            .record_terminal_by_agent(
                "auth@aaa",
                AgentFanoutSlotStatus::Failed,
                Some("child failed".into()),
            )
            .unwrap();
        assert_eq!(
            fanout_get_results_status_label(&failed),
            "completed_with_issues"
        );

        let mut stopped_by_user = AgentFanoutGroupProjection::new("review-3", "Review", 1);
        stopped_by_user
            .record_spawn_accepted(0, "auth@aaa")
            .unwrap();
        stopped_by_user
            .record_terminal_by_agent(
                "auth@aaa",
                AgentFanoutSlotStatus::CancelledByUser,
                Some("user-requested".into()),
            )
            .unwrap();
        assert_eq!(
            fanout_get_results_status_label(&stopped_by_user),
            "completed_with_issues"
        );
    }

    #[test]
    fn fanout_group_json_exposes_uncollected_terminal_count() {
        let mut group = AgentFanoutGroupProjection::new("review-1", "Review fanout", 2);
        group.record_spawn_accepted(0, "auth@aaa").unwrap();
        group.record_spawn_accepted(1, "api@bbb").unwrap();
        group
            .record_terminal_by_agent("auth@aaa", AgentFanoutSlotStatus::Completed, None)
            .unwrap();
        group
            .record_terminal_by_agent("api@bbb", AgentFanoutSlotStatus::Failed, None)
            .unwrap();

        let value = fanout_group_to_json(&group);

        assert_eq!(value["terminal"], 2);
        assert_eq!(value["collected"], 0);
        assert_eq!(value["uncollected"], 2);
    }

    #[test]
    fn fanout_group_json_keeps_slot_run_identity_for_recovery() {
        let mut group = AgentFanoutGroupProjection::new("review-identity", "Review", 1);
        group
            .record_spawn_accepted_with_run(0, "reviewer@run-reviewer", Some("run-reviewer".into()))
            .unwrap();

        let value = fanout_group_to_json(&group);

        assert_eq!(value["slots"][0]["agent_id"], "reviewer@run-reviewer");
        assert_eq!(value["slots"][0]["run_id"], "run-reviewer");
    }

    #[tokio::test]
    async fn handle_spawn_agent_tool_preserves_explicit_model_override() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor.clone());
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let args = json!({
            "description": "Code quality review",
            "prompt": "Review the latest commit",
            "agent_type": "general-purpose",
            "model": "claude-sonnet-4.6"
        });

        let result = handle_agent_spawn_action(&args, Some(&ctx)).await;

        let completed = collect_spawn_receipt(&result, &ctx).await;
        assert_eq!(completed["status"], "completed", "{completed}");
        assert_eq!(
            executor.take_captured_model().as_deref(),
            Some("claude-sonnet-4.6")
        );
    }

    #[tokio::test]
    async fn handle_spawn_agent_tool_surfaces_interrupted_sync_result() {
        let spawner = test_spawner(Arc::new(InterruptedSpawnExecutor));
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let args = json!({
            "description": "Code review",
            "prompt": "Review the diff and stop if budget runs out",
            "agent_type": "general-purpose"
        });

        let result = handle_agent_spawn_action(&args, Some(&ctx)).await;
        let value = collect_spawn_receipt(&result, &ctx).await;

        assert_eq!(value["status"], "interrupted");
        assert_eq!(value["finish_reason"], "budget_exhausted");
        assert_eq!(value["result"], "partial review");
        assert_eq!(value["tool_calls"], 2);
    }

    #[tokio::test]
    async fn fanout_start_preserves_parent_budget_interrupt_cause() {
        let spawner = test_spawner(Arc::new(InterruptedSpawnExecutor));
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));

        let result = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "target_count": 1,
                "slots": [{
                    "id": "budget",
                    "description": "Budget-sensitive review",
                    "prompt": "Review until the child budget is exhausted"
                }]
            }),
            Some(&ctx),
        )
        .await;
        let collected_value: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(collected_value["status"], "completed_with_issues");
        assert_eq!(collected_value["cancelled_by_parent_budget"], 1);
        assert_eq!(
            collected_value["cancelled_by_user"], 0,
            "terminal counters use one fixed schema even when a cause is absent"
        );
        assert_eq!(
            collected_value["results"][0]["result"]["status"],
            "interrupted"
        );
        assert_eq!(
            collected_value["results"][0]["result"]["finish_reason"],
            "budget_exhausted"
        );
        assert_eq!(collected_value["incomplete_results"], 1);
        assert_eq!(collected_value["provenance"]["complete_deliverables"], 0);
        assert_eq!(collected_value["provenance"]["incomplete_slots"], 1);
        assert_eq!(collected_value["provenance"]["all_slots_delivered"], false);
        assert!(
            collected_value["instruction"]
                .as_str()
                .is_some_and(|instruction| instruction.contains("0/1")
                    && instruction.contains("parent synthesis")),
            "{collected_value}"
        );
        assert_eq!(
            collected_value["recovery"]["resume_existing_work_before_rerun"],
            true
        );
        assert_eq!(
            collected_value["results"][0]["recovery"]["rerun_policy"],
            "resume_existing_agent_or_report_incomplete"
        );
    }

    #[tokio::test]
    async fn fanout_reports_execution_incomplete_child_as_an_issue_not_success() {
        let spawner = test_spawner(Arc::new(ExecutionIncompleteSpawnExecutor));
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));

        let result = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "target_count": 1,
                "slots": [{
                    "id": "news",
                    "description": "Fetch one headline",
                    "prompt": "Fetch one current headline"
                }]
            }),
            Some(&ctx),
        )
        .await;
        let collected: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(collected["status"], "completed_with_issues");
        assert_eq!(collected["completed"], 0);
        assert_eq!(collected["interrupted"], 1);
        assert_eq!(
            collected["results"][0]["result"]["finish_reason"],
            "execution_incomplete"
        );
        assert_eq!(collected["results"][0]["result"]["status"], "interrupted");
    }

    #[tokio::test]
    async fn agent_fanout_malformed_args_returns_unexecuted_structured_advisory() {
        let result = handle_agent_fanout_tool(
            &json!({"_parse_error": {"kind": "invalid_json", "executed": false}}),
            None,
        )
        .await;
        let value: Value = serde_json::from_str(&result).expect("fanout error must stay JSON");
        assert_eq!(value["status"], "failed");
        assert_eq!(
            value["error_kind"],
            astra_core::ErrorKind::ToolInvalidArgs.as_str()
        );
        assert_eq!(value["advisory"]["kind"], "malformed_tool_arguments");
        assert_eq!(value["advisory"]["tool"], "agent_fanout");
        assert_eq!(value["advisory"]["executed"], false);
    }

    #[tokio::test]
    async fn handle_spawn_agent_tool_marks_executor_dropped_non_retryable() {
        let spawner = test_spawner(Arc::new(ExecutorDroppedExecutor));
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let args = json!({
            "description": "Code review",
            "prompt": "Review the diff",
            "agent_type": "general-purpose"
        });

        let result = handle_agent_spawn_action(&args, Some(&ctx)).await;
        let value = collect_spawn_receipt(&result, &ctx).await;

        assert_eq!(value["status"], "failed");
        assert_eq!(value["finish_reason"], "executor_dropped");
        assert_eq!(value["retryable"], false);
        assert!(
            value["instruction"]
                .as_str()
                .is_some_and(|instruction| instruction.contains("Do not retry the agent spawn")),
            "{result}"
        );
        assert_eq!(
            value["diagnostic"], "executor_dropped",
            "executor_dropped output must carry a structured diagnostic field, got {result}"
        );
        assert_eq!(
            value["agent_id"], value["agent_id"],
            "Failed output must carry a structured agent_id, got {result}"
        );
        assert!(
            value["agent_id"].as_str().is_some_and(|id| !id.is_empty()),
            "agent_id must be a non-empty string, got {result}"
        );
    }

    #[tokio::test]
    async fn get_result_includes_fanout_summary_for_completed_slot() {
        let spawner = test_spawner(Arc::new(CapturingModelExecutor::new()));
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let spawn = handle_agent_spawn_action(
            &json!({
                "description": "Storage review",
                "prompt": "Review storage layer",
                "agent_type": "general-purpose",
                "fanout_group_id": "review-1",
                "fanout_target_count": 3,
                "fanout_slot_index": 1,
                "fanout_slot_id": "storage"
            }),
            Some(&ctx),
        )
        .await;
        let spawned: Value = serde_json::from_str(&spawn).unwrap();
        let agent_id = spawned["agent_id"].as_str().unwrap();

        let result =
            handle_agent_get_result_action(&json!({"agent_id": agent_id}), Some(&ctx)).await;
        let value: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(value["status"], "completed");
        assert_eq!(value["fanout"]["group_id"], "review-1");
        assert_eq!(value["fanout"]["target_count"], 3);
        assert_eq!(value["fanout"]["slot_index"], 1);
        assert_eq!(value["fanout"]["id"], "storage");
        assert_eq!(value["fanout"]["completed"], 1);
        assert_eq!(value["fanout"]["all_slots_delivered"], false);
        assert_eq!(value["fanout"]["collected"], 1);
    }

    #[tokio::test(start_paused = true)]
    async fn get_result_is_a_short_snapshot_not_a_two_minute_barrier() {
        let spawner = test_spawner(Arc::new(PendingExecutor));
        let ctx = test_spawn_context(spawner.clone(), Some("MiniMax-M2.7"));
        let spawn_ctx = ctx.clone();
        let spawn_task = tokio::spawn(async move {
            handle_agent_spawn_action(
                &json!({
                    "description": "Long review",
                    "prompt": "Keep reviewing until externally stopped",
                    "agent_type": "general-purpose"
                }),
                Some(&spawn_ctx),
            )
            .await
        });
        for _ in 0..100 {
            if spawner.list_all_agents().await.len() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        let promoted = spawner
            .promote_foreground_work_to_background(Some("run-parent"))
            .await;
        assert_eq!(promoted.len(), 1);
        let spawn = spawn_task.await.expect("foreground spawn task");
        let spawned: Value = serde_json::from_str(&spawn).unwrap();
        assert_eq!(spawned["delivery"], "explicit_background_handoff");
        let agent_id = spawned["agent_id"].as_str().unwrap();

        let result =
            handle_agent_get_result_action(&json!({"agent_id": agent_id}), Some(&ctx)).await;
        let value: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(value["status"], "still_running");
        assert_eq!(value["waited_secs"], 1);
        assert_eq!(value["delivery"], "asynchronous_parent_mailbox");
        assert!(
            value["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("Do not busy-poll")),
            "{value}"
        );
    }

    #[tokio::test]
    async fn get_result_includes_fanout_summary_for_user_cancelled_slot() {
        let spawner = test_spawner(Arc::new(PendingExecutor));
        let ctx = test_spawn_context(spawner.clone(), Some("MiniMax-M2.7"));
        let spawn_ctx = ctx.clone();
        let spawn_task = tokio::spawn(async move {
            handle_agent_spawn_action(
                &json!({
                    "description": "Storage review",
                    "prompt": "Review storage layer",
                    "agent_type": "general-purpose",
                    "fanout_group_id": "review-1",
                    "fanout_target_count": 3,
                    "fanout_slot_index": 1
                }),
                Some(&spawn_ctx),
            )
            .await
        });
        for _ in 0..100 {
            if spawner.list_all_agents().await.len() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            spawner
                .promote_foreground_work_to_background(Some("run-parent"))
                .await
                .len(),
            1
        );
        let spawn = spawn_task.await.expect("foreground spawn task");
        let spawned: Value = serde_json::from_str(&spawn).unwrap();
        assert_eq!(spawned["status"], "launched", "{spawn}");
        let agent_id = spawned["agent_id"].as_str().unwrap();

        assert!(
            spawner
                .cancel_agent(agent_id, "user-requested via test")
                .await
        );
        let result =
            handle_agent_get_result_action(&json!({"agent_id": agent_id}), Some(&ctx)).await;
        let value: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(value["status"], "cancelled");
        assert_eq!(value["cancelled_by_user"], true);
        assert_eq!(value["fanout"]["target_count"], 3);
        assert_eq!(value["fanout"]["cancelled_by_user"], 1);
        assert_eq!(value["fanout"]["active"], 0);
        assert!(
            value["instruction"]
                .as_str()
                .is_some_and(|text| text.contains("Do NOT respawn")),
            "{value}"
        );
    }

    #[tokio::test]
    async fn get_result_fanout_summary_does_not_report_non_budget_interruption_as_parent_budget() {
        let spawner = test_spawner(Arc::new(EmptyCompletionExecutor));
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let spawn = handle_agent_spawn_action(
            &json!({
                "description": "Storage review",
                "prompt": "Review storage layer",
                "agent_type": "general-purpose",
                "fanout_group_id": "review-1",
                "fanout_target_count": 3,
                "fanout_slot_index": 1
            }),
            Some(&ctx),
        )
        .await;
        let spawned: Value = serde_json::from_str(&spawn).unwrap();
        let agent_id = spawned["agent_id"].as_str().unwrap();

        let result =
            handle_agent_get_result_action(&json!({"agent_id": agent_id}), Some(&ctx)).await;
        let value: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(value["finish_reason"], "empty_completion");
        assert_eq!(value["fanout"]["target_count"], 3);
        assert_eq!(value["fanout"]["failed"], 0);
        assert_eq!(value["fanout"]["interrupted"], 1);
        assert_eq!(value["fanout"]["cancelled_by_parent_budget"], 0);
        assert!(
            value["fanout"]["summary"]
                .as_str()
                .is_some_and(|summary| summary.contains("1 interrupted")
                    && !summary.contains("cancelled by parent budget")),
            "{value}"
        );
    }

    #[tokio::test]
    async fn get_agent_result_missing_agent_id() {
        let result = handle_agent_get_result_action(&json!({}), None).await;
        assert!(result.contains("Missing required field"));
        assert!(result.contains("\"status\":\"failed\""), "{result}");
    }

    #[tokio::test]
    async fn get_agent_result_no_context() {
        let result = handle_agent_get_result_action(&json!({"agent_id": "child-1"}), None).await;
        assert!(
            result.contains("multi-agent runtime is not connected"),
            "{result}"
        );
        assert!(result.contains("tool_search"), "{result}");
        assert!(result.contains(astra_core::ErrorKind::ToolBinding.as_str()));
        assert!(result.contains("\"status\":\"failed\""), "{result}");
    }

    #[tokio::test]
    async fn get_agent_result_unknown_agent_id_fails_explicitly() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor);
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));

        let result =
            handle_agent_get_result_action(&json!({"agent_id": "security-review"}), Some(&ctx))
                .await;
        let value: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(value["status"], "failed");
        assert_eq!(value["agent_id"], "security-review");
        assert!(
            value["error"]
                .as_str()
                .unwrap_or("")
                .contains("exact runtime-generated agent_id"),
            "{result}"
        );
        assert!(
            value["error"].as_str().unwrap_or("").contains("name"),
            "{result}"
        );
    }

    #[tokio::test]
    async fn get_agent_result_rejects_overlong_agent_id_without_echoing_it() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor);
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let huge = "a".repeat(MAX_AGENT_ID_BYTES + 1);

        let result = handle_agent_get_result_action(&json!({"agent_id": huge}), Some(&ctx)).await;
        let value: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(value["status"], "failed");
        let expected_msg = format!("exceeds {MAX_AGENT_ID_BYTES} bytes");
        assert!(
            value["error"]
                .as_str()
                .unwrap_or("")
                .contains(&expected_msg),
            "{result}"
        );
        // No fragment of the oversized id should appear anywhere in the
        // returned tool output (neither prose nor the structured field).
        // 16 bytes is short enough to be a sensitive prefix and long enough
        // that it cannot occur incidentally.
        assert!(
            !result.contains(&"a".repeat(16)),
            "error must not echo attacker-controlled oversized ids: {result}"
        );
    }

    #[tokio::test]
    async fn shared_handler_rejects_send_message_without_mailbox_executor() {
        let result = handle_agent_tool(
            &json!({
                "action": "send_message",
                "to": "agent-1",
                "message": "hello"
            }),
            None,
        )
        .await;
        let value: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(value["status"], "failed");
        let error = value["error"].as_str().unwrap_or("");
        assert!(
            error.contains("multi-agent runtime is not connected"),
            "{result}"
        );
        assert_eq!(
            value["error_kind"].as_str(),
            Some(astra_core::ErrorKind::ToolBinding.as_str())
        );
    }

    #[tokio::test]
    async fn shared_handler_classifies_missing_spawn_binding() {
        let result = handle_agent_tool(
            &json!({
                "action": "spawn",
                "description": "Review",
                "prompt": "Review the patch."
            }),
            None,
        )
        .await;
        let value: Value = serde_json::from_str(&result).unwrap();
        let error = value["error"].as_str().unwrap_or("");
        assert!(
            error.contains("multi-agent runtime is not connected"),
            "{result}"
        );
        assert_eq!(
            value["error_kind"].as_str(),
            Some(astra_core::ErrorKind::ToolBinding.as_str())
        );
    }
}
