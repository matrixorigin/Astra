//! Shared handler for the consolidated `agent` tool.
//!
//! CLI and server execution environments own different child-loop
//! executors, but the `agent(action='spawn'|'get_result')` contract is
//! runtime semantics. Keep parsing, normalization, lifecycle dispatch, and
//! result rendering here so Web/server cannot drift from CLI behavior.

use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
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
/// Per-slot result byte limit in aggregate `get_results`. Individual
/// `get_result` calls are unbounded; this only caps the combined response.
const MAX_FANOUT_SLOT_RESULT_BYTES: usize = 30_000;
/// Total aggregate byte limit for the combined `results[]` array in
/// `get_results`/start-that-completed. If exceeded, per-slot limits
/// are proportionally reduced until the total fits.
const MAX_FANOUT_AGGREGATE_BYTES: usize = 60_000;
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
            "Invalid agent call shape. Use the top-level `action='spawn'` field, not a `spawn` wrapper key. Example: agent(action='spawn', description='...', prompt='...').",
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
const FANOUT_GET_RESULTS_FIELDS: &[&str] = &["action", "_tool_call_id", "group_id"];
const FANOUT_STOP_SLOT_FIELDS: &[&str] = &["action", "_tool_call_id", "group_id", "slot_index"];
const FANOUT_START_SHAPE: &str = "Use canonical shape: agent_fanout(action='start', target_count=N, slots=[{id:'api', description:'Short UI label', prompt:'Full child task prompt'}], defaults={agent_type:'...', model:'...'}). Put work instructions in each slots[i].prompt; there is no top-level brief or agents payload. Runtime config (agent_type, model, max_turns, etc.) belongs in `defaults`, not at top level. Backgrounding is user-controlled with Ctrl+B; do not pass run_in_background.";
const FANOUT_GET_RESULTS_SHAPE: &str =
    "Use canonical shape: agent_fanout(action='get_results', group_id='<returned group_id>').";
const FANOUT_STOP_SLOT_SHAPE: &str = "Use canonical shape: agent_fanout(action='stop_slot', group_id='<returned group_id>', slot_index=0).";

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
            return render_agent_tool_error(None, "Agent spawning not available in this context.");
        }
    };
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
    let slots = std::mem::take(&mut input.slots);
    let tool_call_id = input._tool_call_id.clone();

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
                    "status": rendered_value.get("status").cloned().unwrap_or(Value::Null),
                    "error": rendered_value.get("error").cloned().unwrap_or(Value::Null),
                })
            })
        })
        .collect();
    let mut agents: Vec<Value> = join_all(futs).await;
    // Restore slot-index order.
    agents.sort_by_key(|v| v.get("slot_index").and_then(Value::as_u64).unwrap_or(0));

    // If any agent is still running asynchronously, return a lightweight
    // "started" response with spawn status only (no full results yet).
    let any_launched = agents
        .iter()
        .any(|agent| agent.get("status").and_then(Value::as_str) == Some("launched"));
    if any_launched {
        let group = find_fanout_group(ctx, &group_id).await;
        return json!({
            "status": "started",
            "group_id": group_id,
            "title": title,
            "target_count": input.target_count,
            "agents": agents,
            "fanout": group.as_ref().map(fanout_group_to_json).unwrap_or(Value::Null),
        })
        .to_string();
    }

    // Detect user-interrupted fanout: all slots failed with the same
    // "agent task ended" error = the parent future was dropped (Ctrl+G).
    // Return an explicit anti-retry signal instead of generic failure.
    let all_failed_same = !agents.is_empty()
        && agents.iter().all(|a| {
            a.get("error")
                .and_then(Value::as_str)
                .is_some_and(|e| e.contains("agent task ended before returning"))
        });
    if all_failed_same {
        return json!({
            "status": "interrupted",
            "group_id": group_id,
            "title": title,
            "target_count": input.target_count,
            "cancelled_by_user": true,
            "instruction": "All agents in this fanout were interrupted (likely by user Ctrl+G). Do NOT retry or respawn. Ask the user what to do next.",
        })
        .to_string();
    }

    // All agents completed synchronously — return the full results directly.
    // No separate "agents[]" field: results[] already contains status per slot.
    render_agent_fanout_results(ctx, &group_id, tool_call_id).await
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
            return render_agent_tool_error(None, "Agent spawning not available in this context.");
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
    render_agent_fanout_results(ctx, group_id, input._tool_call_id).await
}

async fn render_agent_fanout_results(
    ctx: &AgentToolContext,
    group_id: &str,
    tool_call_id: Option<String>,
) -> String {
    let Some(group) = find_fanout_group(ctx, group_id).await else {
        return render_agent_tool_error(None, &format!("Unknown fanout group_id: {group_id}"));
    };

    let mut results: Vec<Value> = Vec::with_capacity(group.slots.len());
    let mut futs: Vec<_> = Vec::new();

    for slot in &group.slots {
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
        let mut get_args = json!({ "agent_id": agent_id });
        if let Some(tool_call_id) = tool_call_id {
            get_args
                .as_object_mut()
                .expect("get_result args object")
                .insert("_tool_call_id".to_string(), Value::String(tool_call_id));
        }
        futs.push(Box::pin(async move {
            let rendered = handle_agent_get_result_action(&get_args, Some(ctx)).await;
            let mut value = serde_json::from_str::<Value>(&rendered)
                .unwrap_or_else(|_| json!({ "status": "failed", "error": rendered }));
            // Truncate oversized results in the aggregate response.
            if let Some(result_field) = value.get("result").and_then(Value::as_str) {
                if result_field.len() > MAX_FANOUT_SLOT_RESULT_BYTES {
                    let truncated =
                        truncate_str_at_char_boundary(result_field, MAX_FANOUT_SLOT_RESULT_BYTES);
                    value["result"] = json!(format!(
                        "{}\n\n[truncated — {} bytes total; use agent(action='get_result', agent_id='{}') for full output]",
                        truncated,
                        result_field.len(),
                        agent_id,
                    ));
                }
            }
            json!({
                "slot_index": slot_index,
                "id": slot_id,
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

    // Enforce total aggregate byte budget: if the combined results exceed
    // MAX_FANOUT_AGGREGATE_BYTES, re-truncate per-slot proportionally.
    let serialized_total: usize = results.iter().map(|v| v.to_string().len()).sum();
    if serialized_total > MAX_FANOUT_AGGREGATE_BYTES && !results.is_empty() {
        let per_slot_budget = MAX_FANOUT_AGGREGATE_BYTES / results.len();
        for item in &mut results {
            if let Some(result_obj) = item.get("result") {
                let result_str = result_obj.to_string();
                if result_str.len() > per_slot_budget {
                    let agent_id = item
                        .get("agent_id")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let truncated = truncate_str_at_char_boundary(&result_str, per_slot_budget);
                    item["result"] = json!(format!(
                        "{}\n\n[truncated — {} bytes total; use agent(action='get_result', agent_id='{}') for full output]",
                        truncated,
                        result_str.len(),
                        agent_id,
                    ));
                }
            }
        }
    }

    let updated = find_fanout_group(ctx, group_id).await.unwrap_or(group);
    let summary = updated.summary();
    let mut response = json!({
        "status": fanout_get_results_status_label(&updated),
        "group_id": group_id,
        "title": updated.title,
        "target_count": updated.target_count,
        "results": results,
    });
    let obj = response.as_object_mut().unwrap();
    if summary.completed > 0 {
        obj.insert("completed".into(), json!(summary.completed));
    }
    if summary.failed > 0 {
        obj.insert("failed".into(), json!(summary.failed));
    }
    if summary.cancelled_by_user > 0 {
        obj.insert("cancelled_by_user".into(), json!(summary.cancelled_by_user));
    }
    if summary.spawn_rejected > 0 {
        obj.insert("spawn_rejected".into(), json!(summary.spawn_rejected));
    }
    if summary.timed_out > 0 {
        obj.insert("timed_out".into(), json!(summary.timed_out));
    }
    response.to_string()
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
            return render_agent_tool_error(None, "Agent spawning not available in this context.");
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
    insert_optional_u32(
        object,
        "max_turns",
        slot.max_turns
            .or_else(|| defaults.and_then(|d| d.max_turns)),
    );
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
            "id": &slot.slot_id,
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
            "unsupported `run_in_background` field for `agent(action='spawn')`. Backgrounding is a user-controlled UI action: omit this field and let the user press Ctrl+B while the live agent is running."
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
    fn spawn_arg_normalization_rejects_run_in_background_field() {
        let err = normalize_agent_spawn_args(&json!({
            "action": "spawn",
            "description": "Review one",
            "prompt": "p1",
            "run_in_background": true,
            "_tool_call_id": "call-1"
        }))
        .expect_err("model-facing spawn must not background itself");
        assert!(err.contains("run_in_background"), "{err}");
        assert!(err.contains("Ctrl+B"), "{err}");
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
        let value: Value = serde_json::from_str(&result).unwrap();

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
        assert_eq!(groups[0].slots[0].slot_id.as_deref(), Some("storage"));
        assert_eq!(groups[0].slots[1].slot_id.as_deref(), Some("ui"));
    }

    #[tokio::test]
    async fn agent_fanout_start_defaults_to_foreground_results() {
        let spawner = test_spawner(Arc::new(CapturingModelExecutor::new()));
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let result = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "_tool_call_id": "call-foreground",
                "group_id": "review-foreground",
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
        assert_eq!(value["group_id"], "review-foreground");
        assert_eq!(value["results"].as_array().unwrap().len(), 1);
        assert_eq!(value["results"][0]["id"], "storage");
        assert_eq!(value["results"][0]["result"]["status"], "completed");
        assert_eq!(value["completed"], 1);
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
        assert!(start.contains("\"status\":\"completed\""), "{start}");

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
        assert!(start.contains("\"status\":\"failed_to_start\""), "{start}");
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
        assert!(start.contains("\"status\":\"failed\""), "{start}");

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
        assert_eq!(value["failed"], 1);
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
        assert!(start.contains("\"status\":\"completed\""), "{start}");

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
        let ctx_for_start = ctx.clone();
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
                Some(&ctx_for_start),
            )
            .await
        });

        for _ in 0..50 {
            if !spawner.list_agents("run-parent").await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let promoted = spawner
            .promote_foreground_agent_to_background(Some("run-parent"))
            .await
            .expect("Ctrl+B promotion should background the running fanout slot");
        assert!(promoted.run_in_background);

        let start = start_task.await.expect("fanout start task should join");
        assert!(start.contains("\"status\":\"started\""), "{start}");

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

        let result = handle_agent_fanout_tool(
            &json!({"action": "get_results", "group_id": "review-atomic"}),
            Some(&ctx),
        )
        .await;
        let value: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(value["status"], "finished");
        assert_eq!(value["cancelled_by_user"], 1);
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
        assert_eq!(value["fanout"]["collected"], 1);
    }

    #[tokio::test]
    async fn get_result_includes_fanout_summary_for_user_cancelled_slot() {
        let spawner = test_spawner(Arc::new(PendingExecutor));
        let ctx = test_spawn_context(spawner.clone(), Some("MiniMax-M2.7"));
        let ctx_for_spawn = ctx.clone();
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
                Some(&ctx_for_spawn),
            )
            .await
        });

        for _ in 0..50 {
            if !spawner.list_agents("run-parent").await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        spawner
            .promote_foreground_agent_to_background(Some("run-parent"))
            .await
            .expect("Ctrl+B promotion should background the running fanout slot");

        let spawn = spawn_task.await.expect("spawn task should join");
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
