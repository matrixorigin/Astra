//! Shared handler for the consolidated `agent` tool.
//!
//! CLI and server execution environments own different child-loop
//! executors, but the `agent(action='spawn'|'get_result')` contract is
//! runtime semantics. Keep parsing, normalization, lifecycle dispatch, and
//! result rendering here so Web/server cannot drift from CLI behavior.

use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::join_all;

use astra_turn_core::orchestration::agent_result_wire::{
    render_agent_tool_error, render_unknown_agent_result, render_wait_for_agent_status,
    render_wait_timeout_outcome,
};
use astra_turn_core::orchestration_fanout_group::{
    AgentFanoutGroupProjection, AgentFanoutSlotStatus, AgentFanoutStatus,
};

use super::{
    DynamicAgentSpawner, InheritedPermissions, SpawnAgentInput, SpawnContext, WaitForAgentOutcome,
};
use astra_turn_core::trace_event::TraceContext;

/// Maximum byte length we accept for an `agent_id` argument before
/// rejecting the request without echoing the value. Bytes (not chars)
/// because the limit is really about prompt-injection / log-bloat budget.
const MAX_AGENT_ID_BYTES: usize = 256;
static NEXT_FANOUT_GROUP_ID: AtomicU64 = AtomicU64::new(1);
/// Static prose for the `Unknown` outcome. Must NOT interpolate the
/// caller-supplied agent_id — that value already appears in the
/// structured `agent_id` JSON field, where serde escapes it safely.
const UNKNOWN_AGENT_ID_ERROR: &str = "Unknown agent_id. Use the exact runtime-generated agent_id returned by the earlier spawn result. The optional spawn `name` is only for send_message addressing and cannot be used with get_result.";

/// Context for executing `agent` tool lifecycle actions.
#[derive(Clone)]
pub struct AgentToolContext {
    /// Current agent's run ID.
    pub run_id: String,
    /// Current agent's ID.
    pub agent_id: String,
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
    /// DB trace identity shared with the current Web turn.
    pub trace_context: Option<TraceContext>,
    /// UI/runtime execution binding metadata inherited by child agents.
    pub execution_metadata: Option<Value>,
}

/// Handle the consolidated `agent` tool for shared dynamic-agent actions.
///
/// Environment-specific actions such as `run_chain` can still be handled by
/// the caller before/after this function. This shared handler intentionally
/// owns spawn/get_result validation and rendering.
pub async fn handle_agent_tool(args: &Value, ctx: Option<&AgentToolContext>) -> String {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    match action {
        "spawn" => handle_agent_spawn_action(args, ctx).await,
        "get_result" => handle_agent_get_result_action(args, ctx).await,
        "send_message" => render_agent_tool_error(
            None,
            "agent.send_message requires a mailbox-aware executor and is not handled by the shared spawn/get_result runtime handler.",
        ),
        other if other.is_empty() && args.get("spawn").is_some() => render_agent_tool_error(
            None,
            "Invalid agent call shape. Use the top-level `action='spawn'` field, not a `spawn` wrapper key. Example: agent(action='spawn', description='...', prompt='...', run_in_background: true).",
        ),
        other if other.is_empty() && args.get("agents").is_some() => render_agent_tool_error(
            None,
            "Unsupported `agents` batch payload for `agent`. Each `agent(action='spawn', ...)` call launches exactly one child. Use `agent_fanout(action='start', target_count=N, slots=[...])` for atomic parallel fan-out.",
        ),
        other => render_agent_tool_error(
            None,
            &format!("Unknown agent action: '{other}'. Use one of: spawn, get_result, run_chain"),
        ),
    }
}

/// Handle the atomic `agent_fanout` tool.
pub async fn handle_agent_fanout_tool(args: &Value, ctx: Option<&AgentToolContext>) -> String {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    match action {
        "start" => handle_agent_fanout_start_action(args, ctx).await,
        "get_results" => handle_agent_fanout_get_results_action(args, ctx).await,
        "stop_slot" => handle_agent_fanout_stop_slot_action(args, ctx).await,
        "" => render_agent_tool_error(
            None,
            "Missing required field: action. Use one of: start, get_results, stop_slot",
        ),
        other => render_agent_tool_error(
            None,
            &format!(
                "Unknown agent_fanout action: '{other}'. Use one of: start, get_results, stop_slot"
            ),
        ),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentFanoutStartInput {
    #[serde(default, rename = "action")]
    _action: Option<String>,
    #[serde(default)]
    group_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    target_count: usize,
    slots: Vec<AgentFanoutStartSlot>,
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
struct AgentFanoutStartSlot {
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
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentFanoutGroupInput {
    #[serde(default, rename = "action")]
    _action: Option<String>,
    group_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentFanoutStopSlotInput {
    #[serde(default, rename = "action")]
    _action: Option<String>,
    group_id: String,
    slot_index: usize,
}

async fn handle_agent_fanout_start_action(args: &Value, ctx: Option<&AgentToolContext>) -> String {
    let ctx = match ctx {
        Some(c) => c,
        None => {
            return render_agent_tool_error(None, "Agent spawning not available in this context.");
        }
    };
    let mut input: AgentFanoutStartInput = match serde_json::from_value(args.clone()) {
        Ok(input) => input,
        Err(e) => {
            return render_agent_tool_error(None, &format!("Invalid input: {e}"));
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

    let slots = std::mem::take(&mut input.slots);

    // Validate all slots before spawning any.
    for (slot_index, slot) in slots.iter().enumerate() {
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

    // Spawn all slots concurrently — no head-of-line blocking.
    let futs: Vec<_> = slots
        .into_iter()
        .enumerate()
        .map(|(slot_index, slot)| {
            let spawn_args = fanout_slot_spawn_args(
                &input,
                slot,
                &group_id,
                &title,
                input.target_count,
                slot_index,
                args.get("_tool_call_id").and_then(Value::as_str),
            );
            Box::pin(async move {
                let rendered = handle_agent_spawn_action(&spawn_args, Some(ctx)).await;
                let rendered_value = serde_json::from_str::<Value>(&rendered)
                    .unwrap_or_else(|_| json!({ "status": "failed", "error": rendered }));
                json!({
                    "slot_index": slot_index,
                    "agent_id": rendered_value.get("agent_id").cloned().unwrap_or(Value::Null),
                    "status": rendered_value.get("status").cloned().unwrap_or(Value::Null),
                    "error": rendered_value.get("error").cloned().unwrap_or(Value::Null),
                })
            })
        })
        .collect();
    let mut agents: Vec<Value> = join_all(futs).await;
    // Restore slot-index order.
    agents.sort_by_key(|v| v.get("slot_index").and_then(Value::as_u64).unwrap_or(0));

    let group = find_fanout_group(ctx, &group_id).await;
    json!({
        "status": "started",
        "group_id": group_id,
        "title": title,
        "target_count": input.target_count,
        "agents": agents,
        "fanout": group.as_ref().map(fanout_group_to_json).unwrap_or(Value::Null),
    })
    .to_string()
}

async fn handle_agent_fanout_get_results_action(
    args: &Value,
    ctx: Option<&AgentToolContext>,
) -> String {
    let ctx = match ctx {
        Some(c) => c,
        None => {
            return render_agent_tool_error(None, "Agent spawning not available in this context.");
        }
    };
    let input: AgentFanoutGroupInput = match serde_json::from_value(args.clone()) {
        Ok(input) => input,
        Err(e) => {
            return render_agent_tool_error(None, &format!("Invalid input: {e}"));
        }
    };
    let group_id = input.group_id.trim();
    if group_id.is_empty() {
        return render_agent_tool_error(None, "Invalid input: group_id must be non-empty");
    }
    let Some(group) = find_fanout_group(ctx, group_id).await else {
        return render_agent_tool_error(None, &format!("Unknown fanout group_id: {group_id}"));
    };

    let mut results: Vec<Value> = Vec::with_capacity(group.slots.len());
    let mut futs: Vec<_> = Vec::new();

    for slot in &group.slots {
        let Some(agent_id) = slot.agent_id.as_deref() else {
            results.push(json!({
                "slot_index": slot.slot_index,
                "status": fanout_slot_status_label(slot.status),
                "error": slot.terminal_reason,
            }));
            continue;
        };
        let agent_id = agent_id.to_string();
        let slot_index = slot.slot_index;
        let get_args = json!({
            "agent_id": agent_id,
            "_tool_call_id": args.get("_tool_call_id").and_then(Value::as_str),
        });
        futs.push(Box::pin(async move {
            let rendered = handle_agent_get_result_action(&get_args, Some(ctx)).await;
            let value = serde_json::from_str::<Value>(&rendered)
                .unwrap_or_else(|_| json!({ "status": "failed", "error": rendered }));
            json!({
                "slot_index": slot_index,
                "agent_id": agent_id,
                "result": value,
            })
        }));
    }

    // Query all agent results concurrently.
    let mut concurrent: Vec<Value> = join_all(futs).await;
    results.append(&mut concurrent);
    // Restore slot-index order.
    results.sort_by_key(|v| v.get("slot_index").and_then(Value::as_u64).unwrap_or(0));

    let updated = find_fanout_group(ctx, group_id).await.unwrap_or(group);
    json!({
        "status": fanout_get_results_status_label(&updated),
        "group_id": group_id,
        "target_count": updated.target_count,
        "results": results,
        "fanout": fanout_group_to_json(&updated),
    })
    .to_string()
}

async fn handle_agent_fanout_stop_slot_action(
    args: &Value,
    ctx: Option<&AgentToolContext>,
) -> String {
    let ctx = match ctx {
        Some(c) => c,
        None => {
            return render_agent_tool_error(None, "Agent spawning not available in this context.");
        }
    };
    let input: AgentFanoutStopSlotInput = match serde_json::from_value(args.clone()) {
        Ok(input) => input,
        Err(e) => {
            return render_agent_tool_error(None, &format!("Invalid input: {e}"));
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
    let terminal_reason = slot.terminal_reason.clone();
    let Some(agent_id) = slot.agent_id.clone() else {
        return json!({
            "status": "not_stoppable",
            "reason": "no_accepted_agent",
            "group_id": group_id,
            "slot_index": input.slot_index,
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
        "run_in_background": true,
        "fanout_group_id": group_id,
        "fanout_group_title": group_title,
        "fanout_target_count": target_count,
        "fanout_slot_index": slot_index,
    });
    let object = value.as_object_mut().expect("object");
    insert_optional_string(
        object,
        "agent_type",
        slot.agent_type.or_else(|| input.agent_type.clone()),
    );
    insert_optional_string(object, "model", slot.model.or_else(|| input.model.clone()));
    insert_optional_u32(object, "max_turns", slot.max_turns.or(input.max_turns));
    insert_optional_u32(
        object,
        "max_output_tokens",
        slot.max_output_tokens.or(input.max_output_tokens),
    );
    insert_optional_string(
        object,
        "complexity",
        slot.complexity.or_else(|| input.complexity.clone()),
    );
    insert_optional_bool(object, "isolated", slot.isolated.or(input.isolated));
    insert_optional_string(object, "name", slot.name);
    if let Some(allowed_tools) = slot.allowed_tools.or_else(|| input.allowed_tools.clone()) {
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
        "target_count": summary.target_count,
        "status": fanout_group_status_label(group.status),
        "summary": group.summary_sentence(),
        "accepted": summary.accepted,
        "active": summary.active,
        "terminal": summary.terminal,
        "completed": summary.completed,
        "failed": summary.failed,
        "cancelled_by_user": summary.cancelled_by_user,
        "cancelled_by_parent_budget": summary.cancelled_by_parent_budget,
        "timed_out": summary.timed_out,
        "spawn_rejected": summary.spawn_rejected,
        "collected": summary.collected,
        "uncollected": summary.uncollected,
        "slots": group.slots.iter().map(|slot| json!({
            "slot_index": slot.slot_index,
            "role": slot.role,
            "requested_description": slot.requested_description,
            "agent_id": &slot.agent_id,
            "status": fanout_slot_status_label(slot.status),
            "result_collected": slot.result_collected,
            "terminal_reason": &slot.terminal_reason,
        })).collect::<Vec<_>>(),
    })
}

fn fanout_get_results_status_label(group: &AgentFanoutGroupProjection) -> &'static str {
    let summary = group.summary();
    if summary.spawn_rejected > 0 {
        "failed_to_start"
    } else if summary.cancelled_by_parent_budget > 0 {
        "interrupted"
    } else if summary.failed > 0 || summary.timed_out > 0 {
        "failed"
    } else if summary.active > 0 {
        "incomplete"
    } else if summary.cancelled_by_user > 0 {
        "finished"
    } else {
        "completed"
    }
}

fn fanout_group_status_label(status: AgentFanoutStatus) -> &'static str {
    match status {
        AgentFanoutStatus::Planned => "planned",
        AgentFanoutStatus::Running => "running",
        AgentFanoutStatus::Finished => "finished",
        AgentFanoutStatus::Incomplete => "incomplete",
    }
}

fn fanout_slot_status_label(status: AgentFanoutSlotStatus) -> &'static str {
    match status {
        AgentFanoutSlotStatus::Planned => "planned",
        AgentFanoutSlotStatus::SpawnAccepted => "spawn_accepted",
        AgentFanoutSlotStatus::SpawnRejected => "spawn_rejected",
        AgentFanoutSlotStatus::Running => "running",
        AgentFanoutSlotStatus::Completed => "completed",
        AgentFanoutSlotStatus::Failed => "failed",
        AgentFanoutSlotStatus::CancelledByUser => "cancelled_by_user",
        AgentFanoutSlotStatus::CancelledByParentBudget => "cancelled_by_parent_budget",
        AgentFanoutSlotStatus::TimedOut => "timed_out",
    }
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
            return render_agent_tool_error(None, "Agent spawning not available in this context.");
        }
    };

    if input.model.is_none() {
        input.model = ctx.current_model.clone();
    }
    if let Err(e) = input.validate_fanout_metadata() {
        return render_agent_tool_error(None, &format!("Invalid input: {e}"));
    }

    let mut inherited_permissions = ctx.inherited_permissions.clone();
    inherited_permissions.is_background = input.run_in_background;
    let spawn_ctx = SpawnContext {
        parent_run_id: ctx.run_id.clone(),
        parent_agent_id: ctx.agent_id.clone(),
        recursion_depth: ctx.recursion_depth,
        parent_is_fork_child: ctx.is_fork_child,
        working_dir: ctx.working_dir.clone(),
        inherited_permissions: Some(inherited_permissions),
        inherited_skills: ctx.active_skills.clone(),
        live_event_sink: ctx.live_event_sink.clone(),
        trace_context: ctx.trace_context.clone(),
        execution_metadata: ctx.execution_metadata.clone(),
        spawn_tool_call_id: args
            .get("_tool_call_id")
            .and_then(Value::as_str)
            .map(ToString::to_string),
    };

    match ctx.spawner.spawn(input, &spawn_ctx).await {
        Ok(output) => serde_json::to_string(&output)
            .unwrap_or_else(|_| render_agent_tool_error(None, "Failed to serialize output")),
        Err(e) => render_agent_tool_error(None, &e.to_string()),
    }
}

fn non_empty_string(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
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
            return render_agent_tool_error(None, "Agent spawning not available in this context.");
        }
    };

    if agent_id.len() > MAX_AGENT_ID_BYTES {
        return render_agent_tool_error(
            None,
            &format!("Invalid agent_id: exceeds {MAX_AGENT_ID_BYTES} bytes"),
        );
    }

    let timeout = Duration::from_secs(120);
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
            attach_fanout_to_agent_result(render_wait_for_agent_status(agent_id, &status), group)
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
    let slot_index = agent_id.as_deref().and_then(|agent_id| {
        group
            .slots
            .iter()
            .find(|slot| slot.agent_id.as_deref() == Some(agent_id))
            .map(|slot| slot.slot_index)
    });
    let summary = group.summary();
    let group_id = group.group_id.clone();
    let summary_sentence = group.summary_sentence();
    object.insert(
        "fanout".to_string(),
        json!({
            "group_id": group_id,
            "target_count": summary.target_count,
            "slot_index": slot_index,
            "summary": summary_sentence,
            "accepted": summary.accepted,
            "active": summary.active,
            "terminal": summary.terminal,
            "completed": summary.completed,
            "failed": summary.failed,
            "cancelled_by_user": summary.cancelled_by_user,
            "cancelled_by_parent_budget": summary.cancelled_by_parent_budget,
            "spawn_rejected": summary.spawn_rejected,
            "collected": summary.collected,
        }),
    );
    serde_json::to_string(&value).unwrap_or(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestration::{SpawnAgentExecutor, SpawnRunConfig, SpawnRunResult};
    use crate::server::delegation::engine::DelegationTracker;
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
    fn spawn_arg_normalization_accepts_canonical_run_in_background() {
        let normalized = normalize_agent_spawn_args(&json!({
            "action": "spawn",
            "description": "Review one",
            "prompt": "p1",
            "run_in_background": true,
            "_tool_call_id": "call-1"
        }))
        .unwrap();
        assert!(normalized.get("action").is_none(), "{normalized}");
        assert!(normalized.get("_tool_call_id").is_none(), "{normalized}");
        let input: SpawnAgentInput = serde_json::from_value(normalized).unwrap();
        assert!(input.run_in_background);
    }

    #[tokio::test]
    async fn spawn_no_context_fails_explicitly() {
        let args = json!({
            "description": "Test",
            "prompt": "Test prompt"
        });
        let result = handle_agent_spawn_action(&args, None).await;
        assert!(result.contains("not available"));
        assert!(result.contains("\"status\":\"failed\""), "{result}");
    }

    struct CapturingModelExecutor {
        captured_model: Mutex<Option<String>>,
        captured_execution_metadata: Mutex<Option<Value>>,
    }

    impl CapturingModelExecutor {
        fn new() -> Self {
            Self {
                captured_model: Mutex::new(None),
                captured_execution_metadata: Mutex::new(None),
            }
        }

        fn take_captured_model(&self) -> Option<String> {
            self.captured_model.lock().unwrap().take()
        }

        fn take_captured_execution_metadata(&self) -> Option<Value> {
            self.captured_execution_metadata.lock().unwrap().take()
        }
    }

    #[async_trait::async_trait]
    impl SpawnAgentExecutor for CapturingModelExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            *self.captured_model.lock().unwrap() = config.model.clone();
            *self.captured_execution_metadata.lock().unwrap() = config.execution_metadata.clone();
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

    struct InterruptedSpawnExecutor;

    #[async_trait::async_trait]
    impl SpawnAgentExecutor for InterruptedSpawnExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "interrupted".into(),
                finish_reason: "budget_exhausted".into(),
                output: Some("partial review".into()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 2,
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
                output: None,
                error: Some("child failed".into()),
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 1,
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
                output: Some(String::new()),
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

    struct PendingExecutor;

    #[async_trait::async_trait]
    impl SpawnAgentExecutor for PendingExecutor {
        async fn execute(&self, _config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            std::future::pending::<Result<SpawnRunResult, String>>().await
        }
    }

    fn test_spawner(executor: Arc<dyn SpawnAgentExecutor>) -> Arc<DynamicAgentSpawner> {
        let transport = Arc::new(astra_messaging::InProcessTransport::new());
        let tracker = Arc::new(DelegationTracker::new());
        let router = Arc::new(astra_messaging::AgentMailboxRouter::new(transport, tracker));
        Arc::new(DynamicAgentSpawner::new(router).with_executor(executor))
    }

    fn test_spawn_context(
        spawner: Arc<DynamicAgentSpawner>,
        current_model: Option<&str>,
    ) -> AgentToolContext {
        AgentToolContext {
            run_id: "run-parent".into(),
            agent_id: "root-agent".into(),
            current_model: current_model.map(str::to_string),
            recursion_depth: 0,
            is_fork_child: false,
            working_dir: PathBuf::from("."),
            spawner,
            inherited_permissions: InheritedPermissions::auto_approve(),
            active_skills: Vec::new(),
            live_event_sink: None,
            trace_context: None,
            execution_metadata: None,
        }
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

        assert!(result.contains("\"status\":\"completed\""), "{result}");
        assert_eq!(
            executor.take_captured_model().as_deref(),
            Some("MiniMax-M2.7")
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
                "authority": "read_write",
                "fallback_policy": "disabled"
            },
            "executor": {
                "kind": "edge_agent",
                "executor_id": "edge-macbook-1",
                "display_name": "MacBook Pro",
                "transport": "edge_ws",
                "status": "online"
            },
            "transport": "edge_ws",
            "fallback_policy": "disabled"
        }));
        let args = json!({
            "description": "Code quality review",
            "prompt": "Review the latest commit",
            "agent_type": "general-purpose"
        });

        let result = handle_agent_spawn_action(&args, Some(&ctx)).await;

        assert!(result.contains("\"status\":\"completed\""), "{result}");
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
            "run_in_background": true,
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
                        "description": "Review storage",
                        "prompt": "Review storage changes and report correctness bugs.",
                        "agent_type": "code-review"
                    },
                    {
                        "description": "Review UI",
                        "prompt": "Review UI changes and report state bugs.",
                        "agent_type": "code-review"
                    }
                ]
            }),
            Some(&ctx),
        )
        .await;
        let value: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(value["status"], "started");
        assert_eq!(value["group_id"], "review-atomic");
        assert_eq!(value["title"], "review fanout");
        assert_eq!(value["agents"].as_array().unwrap().len(), 2);
        assert_eq!(value["fanout"]["title"], "review fanout");
        assert_eq!(value["fanout"]["target_count"], 2);
        assert_eq!(value["fanout"]["accepted"], 2);
        assert_eq!(value["fanout"]["slots"][0]["slot_index"], 0);
        assert_eq!(value["fanout"]["slots"][1]["slot_index"], 1);

        let groups = spawner.list_fanout_groups().await;
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].group_id, "review-atomic");
        assert_eq!(groups[0].title, "review fanout");
        assert_eq!(groups[0].target_count, 2);
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

    #[test]
    fn fanout_slot_spawn_args_carry_group_title_for_ui_projection() {
        let input = AgentFanoutStartInput {
            _action: Some("start".into()),
            group_id: Some("review-1".into()),
            title: Some("review fanout".into()),
            target_count: 3,
            slots: Vec::new(),
            agent_type: None,
            model: None,
            max_turns: None,
            max_output_tokens: None,
            complexity: None,
            isolated: None,
            allowed_tools: None,
        };
        let slot = AgentFanoutStartSlot {
            description: "Review storage".into(),
            prompt: "Review storage layer".into(),
            agent_type: None,
            model: None,
            max_turns: None,
            max_output_tokens: None,
            complexity: None,
            isolated: None,
            allowed_tools: None,
            name: None,
        };

        let args = fanout_slot_spawn_args(&input, slot, "review-1", "review fanout", 3, 1, None);

        assert_eq!(args["fanout_group_id"], "review-1");
        assert_eq!(args["fanout_group_title"], "review fanout");
        assert_eq!(args["fanout_target_count"], 3);
        assert_eq!(args["fanout_slot_index"], 1);
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
        assert!(start.contains("\"status\":\"started\""), "{start}");

        let result = handle_agent_fanout_tool(
            &json!({"action": "get_results", "group_id": "review-atomic"}),
            Some(&ctx),
        )
        .await;
        let value: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(value["status"], "completed");
        assert_eq!(value["results"].as_array().unwrap().len(), 1);
        assert_eq!(value["results"][0]["result"]["status"], "completed");
        assert_eq!(value["fanout"]["collected"], 1);
        assert_eq!(value["fanout"]["uncollected"], 0);
    }

    #[tokio::test]
    async fn agent_fanout_get_results_preserves_spawn_rejected_slot_status() {
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
        assert!(start.contains("\"status\":\"started\""), "{start}");
        assert!(start.contains("\"spawn_rejected\":1"), "{start}");

        let result = handle_agent_fanout_tool(
            &json!({"action": "get_results", "group_id": "review-atomic"}),
            Some(&ctx),
        )
        .await;
        let value: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(value["status"], "failed_to_start");
        assert_eq!(value["results"].as_array().unwrap().len(), 1);
        assert_eq!(value["results"][0]["slot_index"], 0);
        assert_eq!(value["results"][0]["status"], "spawn_rejected");
        assert!(
            value["results"][0]["error"]
                .as_str()
                .is_some_and(|error| error.contains("unknown agent type")),
            "rejected slot result should preserve the rejection reason: {value}"
        );
        assert_eq!(value["fanout"]["target_count"], 1);
        assert_eq!(value["fanout"]["accepted"], 0);
        assert_eq!(value["fanout"]["spawn_rejected"], 1);
        assert_eq!(value["fanout"]["slots"][0]["status"], "spawn_rejected");
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
        assert!(start.contains("\"status\":\"started\""), "{start}");

        let result = handle_agent_fanout_tool(
            &json!({"action": "get_results", "group_id": "review-atomic"}),
            Some(&ctx),
        )
        .await;
        let value: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(value["status"], "failed");
        assert_eq!(value["results"].as_array().unwrap().len(), 1);
        assert_eq!(value["results"][0]["slot_index"], 0);
        assert_eq!(value["results"][0]["result"]["status"], "failed");
        assert_eq!(value["fanout"]["failed"], 1);
        assert_eq!(value["fanout"]["slots"][0]["status"], "failed");
        assert!(
            value["fanout"]["summary"]
                .as_str()
                .is_some_and(|summary| summary.contains("1 failed")),
            "failed fanout summary should name child failure: {value}"
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
        assert!(start.contains("\"spawn_rejected\":1"), "{start}");

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
        assert!(start.contains("\"status\":\"started\""), "{start}");

        let collected = handle_agent_fanout_tool(
            &json!({"action": "get_results", "group_id": "review-atomic"}),
            Some(&ctx),
        )
        .await;
        assert!(
            collected.contains("\"status\":\"completed\""),
            "{collected}"
        );

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
        assert!(start.contains("\"status\":\"started\""), "{start}");

        let stop = handle_agent_fanout_tool(
            &json!({
                "action": "stop_slot",
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

        let result = handle_agent_fanout_tool(
            &json!({"action": "get_results", "group_id": "review-atomic"}),
            Some(&ctx),
        )
        .await;
        let value: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(value["status"], "finished");
        assert_eq!(value["fanout"]["cancelled_by_user"], 1);
        assert_eq!(value["fanout"]["active"], 0);
        assert_eq!(value["fanout"]["slots"][0]["status"], "cancelled_by_user");
        assert!(
            value["fanout"]["summary"]
                .as_str()
                .is_some_and(|summary| summary.contains("1 stopped by user")),
            "fanout summary should preserve intentional user stop: {value}"
        );
        assert!(
            !value["fanout"]["summary"]
                .as_str()
                .unwrap_or_default()
                .contains("partial agents returned"),
            "{value}"
        );
    }

    #[test]
    fn fanout_get_results_status_names_non_successful_terminal_groups() {
        let mut spawn_rejected = AgentFanoutGroupProjection::new("review-1", "Review", 1);
        spawn_rejected.record_spawn_rejected(0, "quota").unwrap();
        assert_eq!(
            fanout_get_results_status_label(&spawn_rejected),
            "failed_to_start"
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
        assert_eq!(fanout_get_results_status_label(&failed), "failed");

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
            "finished"
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

        assert!(result.contains("\"status\":\"completed\""), "{result}");
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
        let value: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(value["status"], "interrupted");
        assert_eq!(value["finish_reason"], "budget_exhausted");
        assert_eq!(value["result"], "partial review");
        assert_eq!(value["tool_calls"], 2);
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
                "run_in_background": true,
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

        assert_eq!(value["status"], "completed");
        assert_eq!(value["fanout"]["group_id"], "review-1");
        assert_eq!(value["fanout"]["target_count"], 3);
        assert_eq!(value["fanout"]["slot_index"], 1);
        assert_eq!(value["fanout"]["completed"], 1);
        assert_eq!(value["fanout"]["collected"], 1);
    }

    #[tokio::test]
    async fn get_result_includes_fanout_summary_for_user_cancelled_slot() {
        let spawner = test_spawner(Arc::new(PendingExecutor));
        let ctx = test_spawn_context(spawner.clone(), Some("MiniMax-M2.7"));
        let spawn = handle_agent_spawn_action(
            &json!({
                "description": "Storage review",
                "prompt": "Review storage layer",
                "agent_type": "general-purpose",
                "run_in_background": true,
                "fanout_group_id": "review-1",
                "fanout_target_count": 3,
                "fanout_slot_index": 1
            }),
            Some(&ctx),
        )
        .await;
        let spawned: Value = serde_json::from_str(&spawn).unwrap();
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
                "run_in_background": true,
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
        assert_eq!(value["fanout"]["failed"], 1);
        assert_eq!(value["fanout"]["cancelled_by_parent_budget"], 0);
        assert!(
            value["fanout"]["summary"]
                .as_str()
                .is_some_and(|summary| summary.contains("1 failed")
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
        assert!(result.contains("not available"));
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
        assert!(
            value["error"]
                .as_str()
                .unwrap_or("")
                .contains("mailbox-aware executor"),
            "{result}"
        );
    }
}
