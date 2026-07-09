//! Shared handler for the consolidated `agent` tool.
//!
//! CLI and server execution environments own different child-loop
//! executors, but the `agent(action='spawn'|'get_result')` contract is
//! runtime semantics. Keep parsing, normalization, lifecycle dispatch, and
//! result rendering here so Web/server cannot drift from CLI behavior.

use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::future::join_all;

use astra_tools::agent_tool_contract::{
    AgentAction, AgentFanoutAction, agent_action_from_args, agent_fanout_action_from_args,
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
use astra_turn_core::trace_event::TraceContext;

/// Maximum byte length we accept for an `agent_id` argument before
/// rejecting the request without echoing the value. Bytes (not chars)
/// because the limit is really about prompt-injection / log-bloat budget.
const MAX_AGENT_ID_BYTES: usize = 256;
/// Total aggregate byte limit for the combined `results[]` array in
/// `get_results`/start-that-completed. If exceeded, per-slot limits
/// are proportionally reduced until the total fits.
const MAX_FANOUT_AGGREGATE_BYTES: usize = 60_000;
const FANOUT_RESULT_DEFAULT_MAX_BYTES: usize = 8_192;
const FANOUT_RESULT_MAX_BYTES: usize = 65_536;
const FANOUT_CODE_REVIEW_MIN_TURNS: u32 = 30;
static NEXT_FANOUT_GROUP_ID: AtomicU64 = AtomicU64::new(1);
/// Static prose for the `Unknown` outcome. Must NOT interpolate the
/// caller-supplied agent_id — that value already appears in the
/// structured `agent_id` JSON field, where serde escapes it safely.
const UNKNOWN_AGENT_ID_ERROR: &str = "Unknown agent_id. Use the exact runtime-generated agent_id returned by the earlier spawn result. The optional spawn `name` is only for send_message addressing and cannot be used with get_result.";

fn render_spawn_agent_output(output: SpawnAgentOutput) -> String {
    let mut value = match serde_json::to_value(&output) {
        Ok(value) => value,
        Err(_) => return render_agent_tool_error(None, "Failed to serialize output"),
    };
    if value.get("status").and_then(Value::as_str) == Some("failed")
        && value.get("finish_reason").and_then(Value::as_str) == Some("executor_dropped")
        && let Some(object) = value.as_object_mut()
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
    let action = match agent_action_from_args(args) {
        Ok(action) => action,
        Err(error) => return render_agent_tool_contract_error(&error),
    };
    match action {
        AgentAction::Spawn => handle_agent_spawn_action(args, ctx).await,
        AgentAction::GetResult => handle_agent_get_result_action(args, ctx).await,
        AgentAction::SendMessage => render_agent_runtime_binding_error("agent", "send_message"),
        AgentAction::RunChain => render_agent_tool_error(
            None,
            "agent.run_chain is owned by the executor chain engine and cannot be handled by the shared agent lifecycle handler.",
        ),
    }
}

/// Handle the atomic `agent_fanout` tool.
pub async fn handle_agent_fanout_tool(args: &Value, ctx: Option<&AgentToolContext>) -> String {
    let action = match agent_fanout_action_from_args(args) {
        Ok(action) => action,
        Err(error) => {
            if args.get("action").is_none()
                || args.get("action").and_then(Value::as_str) == Some("")
            {
                return render_agent_tool_contract_error(&format!(
                    "{error} Do not retry with empty args {{}}. Choose one of three canonical shapes:\n\
                         {FANOUT_START_SHAPE}\n\
                         {FANOUT_GET_RESULTS_SHAPE}\n\
                         {FANOUT_STOP_SLOT_SHAPE}"
                ));
            }
            return render_agent_tool_contract_error(&error);
        }
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
const FANOUT_START_SHAPE: &str = "Use canonical shape: agent_fanout(action='start', target_count=N, slots=[{id:'api', description:'Short UI label', prompt:'Full child task prompt'}], defaults={agent_type:'...', model:'...'}). Put work instructions in each slots[i].prompt; there is no top-level brief or agents payload. Runtime config (agent_type, model, max_turns, etc.) belongs in `defaults`, not at top level. Backgrounding is user-controlled with Ctrl+B; do not pass run_in_background.";
const FANOUT_GET_RESULTS_SHAPE: &str = "Use canonical shape: agent_fanout(action='get_results', group_id='<returned group_id>'). For large results, read one slot window with agent_fanout(action='get_results', group_id='<returned group_id>', slot_index=0, offset=0, max_bytes=8192).";
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
    // Budget transparency: detect silent max_turns inflation before slots
    // are moved, then surface it in every response branch below.
    let budget_notice = fanout_budget_adjustment_notice(&input);
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
                    "status": rendered_value.get("status").cloned().unwrap_or(Value::Null),
                    "finish_reason": rendered_value.get("finish_reason").cloned().unwrap_or(Value::Null),
                    "error": rendered_value.get("error").cloned().unwrap_or(Value::Null),
                })
            })
        })
        .collect();
    let mut agents: Vec<Value> = join_all(futs).await;
    // Restore slot-index order.
    agents.sort_by_key(|v| v.get("slot_index").and_then(Value::as_u64).unwrap_or(0));
    if budget_notice.is_some() {
        let stored = ctx
            .spawner
            .set_fanout_group_budget_adjustment(&group_id, budget_notice.clone())
            .await;
        if !stored {
            tracing::warn!(
                target: "fanout",
                group_id = %group_id,
                "budget adjustment dropped: group evicted before result aggregation",
            );
        }
    }

    // If any agent is still running asynchronously, return a lightweight
    // "started" response with spawn status only (no full results yet).
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
            "agents": agents,
            "fanout": group.as_ref().map(fanout_group_to_json).unwrap_or(Value::Null),
        });
        if let Some(notice) = &budget_notice {
            resp.as_object_mut()
                .unwrap()
                .insert("budget_adjustment".into(), json!(notice));
        }
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

    // If any slot was rejected at spawn time (e.g. unknown agent type),
    // surface as `failed_to_start` — a distinct signal from runtime failures.
    // The per-agent `status:"failed"` alone is ambiguous; the group's
    // `spawn_rejected` count is the authoritative indicator.
    {
        let group = find_fanout_group(ctx, &group_id).await;
        let spawn_rejected_count = group
            .as_ref()
            .map(|g| g.summary().spawn_rejected)
            .unwrap_or(0);
        if spawn_rejected_count > 0 {
            let mut resp = json!({
                "status": "failed_to_start",
                "group_id": group_id,
                "title": title,
                "target_count": input.target_count,
                "agents": agents,
                "spawn_rejected": spawn_rejected_count,
                "instruction": "One or more fanout slots were rejected at spawn time. Do not retry or respawn replacements. Use agent_fanout(action='get_results', group_id=...) to collect any available partial results.",
            });
            terminal_causes.insert_json_fields(resp.as_object_mut().unwrap());
            if let Some(notice) = &budget_notice {
                resp.as_object_mut()
                    .unwrap()
                    .insert("budget_adjustment".into(), json!(notice));
            }
            return resp.to_string();
        }
    }

    // If every slot returned synchronously but at least one stopped
    // non-successfully, preserve the structured cause counts. This keeps a
    // user cancellation distinct from parent-budget, timeout, executor-drop,
    // and ordinary child failure paths.
    if terminal_causes.has_stopped_slots() {
        let mut resp = json!({
            "status": "interrupted",
            "group_id": group_id,
            "title": title,
            "target_count": input.target_count,
            "agents": agents,
            "instruction": "One or more fanout slots stopped before normal completion. Do not retry or respawn replacements. Use the structured cause counts in this result, collect any available partial output, or ask the user how to proceed.",
        });
        terminal_causes.insert_json_fields(resp.as_object_mut().unwrap());
        if let Some(notice) = &budget_notice {
            resp.as_object_mut()
                .unwrap()
                .insert("budget_adjustment".into(), json!(notice));
        }
        return resp.to_string();
    }

    // All agents completed synchronously — return the full results directly.
    // No separate "agents[]" field: results[] already contains status per slot.
    let mut results = render_agent_fanout_results(
        ctx,
        &group_id,
        tool_call_id,
        FanoutResultReadOptions::default(),
    )
    .await;
    if let Some(notice) = &budget_notice {
        if let Ok(mut value) = serde_json::from_str::<Value>(&results) {
            if let Some(obj) = value.as_object_mut() {
                obj.insert("budget_adjustment".into(), json!(notice));
                results = value.to_string();
            }
        }
    }
    results
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
            let rendered = handle_agent_get_result_action(&get_args, Some(ctx)).await;
            let mut value = serde_json::from_str::<Value>(&rendered)
                .unwrap_or_else(|_| json!({ "status": "failed", "error": rendered }));
            let window = window_fanout_agent_result(
                &mut value,
                &group_id,
                slot_index,
                read_options.offset,
                read_options.max_bytes,
            );
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
    let mut response = json!({
        "status": fanout_get_results_status_label(&updated),
        "group_id": group_id,
        "title": updated.title,
        "target_count": updated.target_count,
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
        "results": results,
    });
    let obj = response.as_object_mut().unwrap();
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
    if summary.completed > 0 {
        obj.insert("completed".into(), json!(summary.completed));
    }
    if summary.failed > 0 {
        obj.insert("failed".into(), json!(summary.failed));
    }
    if summary.interrupted > 0 {
        obj.insert("interrupted".into(), json!(summary.interrupted));
    }
    if summary.cancelled_by_user > 0 {
        obj.insert("cancelled_by_user".into(), json!(summary.cancelled_by_user));
    }
    if summary.spawn_rejected > 0 {
        obj.insert("spawn_rejected".into(), json!(summary.spawn_rejected));
    }
    if let Some(notice) = updated.budget_adjustment.as_ref() {
        obj.insert("budget_adjustment".into(), json!(notice));
    }
    if summary.timed_out > 0 {
        obj.insert("timed_out".into(), json!(summary.timed_out));
    }
    if summary.cancelled_by_parent_budget > 0 {
        obj.insert(
            "cancelled_by_parent_budget".into(),
            json!(summary.cancelled_by_parent_budget),
        );
    }
    // Anti-respawn instruction: prevent LLM from spawning additional agents
    // to retry failed slots. The fanout group is a fixed-size contract;
    // retries inflate the group and corrupt accounting.
    let has_failures = summary.failed > 0
        || summary.interrupted > 0
        || summary.spawn_rejected > 0
        || summary.timed_out > 0
        || summary.cancelled_by_user > 0
        || summary.cancelled_by_parent_budget > 0;
    if has_failures {
        obj.insert(
            "instruction".into(),
            json!(
            "Do NOT retry, respawn, or spawn additional agents to replace failed/interrupted/cancelled slots. \
             The fanout group has a fixed target_count and adding agents corrupts accounting. \
             Work with the results you have, or ask the user how to proceed."
        ),
        );
    } else if summary.active == 0 && summary.terminal == summary.target_count {
        obj.insert(
            "instruction".into(),
            json!(
                "Fanout target_count is complete. Do not call agent(action='spawn') to add, retry, or replace agents in this turn. Present the collected results; ask the user before starting any additional fanout."
            ),
        );
    }
    serde_json::to_string_pretty(&response).unwrap_or_else(|_| response.to_string())
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
    let requested = slot
        .max_turns
        .or_else(|| defaults.and_then(|d| d.max_turns));
    let agent_type = slot
        .agent_type
        .as_deref()
        .or_else(|| defaults.and_then(|d| d.agent_type.as_deref()))
        .map(str::trim)
        .filter(|agent_type| !agent_type.is_empty())
        .unwrap_or("general-purpose");
    let complexity = slot
        .complexity
        .as_deref()
        .or_else(|| defaults.and_then(|d| d.complexity.as_deref()))
        .map(str::trim)
        .map(str::to_ascii_lowercase);

    let is_deep = matches!(complexity.as_deref(), Some("deep" | "thorough" | "heavy"));
    let is_code_review = agent_type == "code-review";
    let agent_default = builtin_agent_default_max_turns(agent_type);
    let min_turns = agent_default.or(if is_deep || is_code_review {
        Some(FANOUT_CODE_REVIEW_MIN_TURNS)
    } else {
        None
    });

    match (requested, min_turns, is_deep) {
        (Some(requested), Some(min_turns), _) => Some(requested.max(min_turns)),
        (None, Some(min_turns), false) => Some(min_turns),
        (None, _, _) => None,
        (Some(requested), None, _) => Some(requested),
    }
}

fn builtin_agent_default_max_turns(agent_type: &str) -> Option<u32> {
    astra_turn_core::orchestration_builtin_agents::get_builtin_agent_types()
        .into_iter()
        .find(|definition| definition.agent_type == agent_type)
        .map(|definition| definition.max_turns)
}

/// Detect whether the effective `max_turns` diverged from what the caller
/// requested, and if so, produce a human-readable transparency notice.
///
/// First principles: a silent budget override breaks the caller's mental model
/// of cost. When we raise a too-small request to the agent-type floor, the
/// caller must be told so the parent agent can report the actual execution
/// shape instead of pretending the smaller budget was used.
fn fanout_budget_adjustment_notice(input: &AgentFanoutStartInput) -> Option<String> {
    let defaults = input.defaults.as_ref();
    let mut adjustments: Vec<String> = Vec::new();
    for (i, slot) in input.slots.iter().enumerate() {
        let requested = slot
            .max_turns
            .or_else(|| defaults.and_then(|d| d.max_turns));
        let effective = fanout_effective_max_turns(slot, defaults);
        match (requested, effective) {
            (Some(req), Some(eff)) if eff > req => {
                let label = slot
                    .slot_id
                    .as_deref()
                    .map(|id| format!("id={id}"))
                    .unwrap_or_else(|| format!("slot[{i}]"));
                adjustments.push(format!("{label}: max_turns {req} → {eff}"));
            }
            _ => {}
        }
    }
    if adjustments.is_empty() {
        None
    } else {
        Some(format!(
            "Budget adjusted — agent-type minimum turn budget enforced ({}).",
            adjustments.join("; ")
        ))
    }
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
    let mut value = json!({
        "group_id": group.group_id,
        "title": group.title,
        "target_count": summary.target_count,
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
            "status": fanout_slot_status_label(slot.status),
            "result_collected": slot.result_collected,
            "terminal_reason": &slot.terminal_reason,
        })).collect::<Vec<_>>(),
    });
    if let Some(notice) = group.budget_adjustment.as_ref() {
        value
            .as_object_mut()
            .unwrap()
            .insert("budget_adjustment".into(), json!(notice));
    }
    value
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

    let spawn_ctx = SpawnContext {
        parent_run_id: ctx.run_id.clone(),
        parent_agent_id: ctx.agent_id.clone(),
        recursion_depth: ctx.recursion_depth,
        parent_is_fork_child: ctx.is_fork_child,
        working_dir: ctx.working_dir.clone(),
        inherited_permissions,
        inherited_skills: ctx.active_skills.clone(),
        live_event_sink: ctx.live_event_sink.clone(),
        trace_context: ctx.trace_context.clone(),
        execution_metadata: ctx.execution_metadata.clone(),
        spawn_tool_call_id: args
            .get("_tool_call_id")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        delegation_chain: child_delegation_chain,
    };

    match ctx.spawner.spawn(input, &spawn_ctx).await {
        Ok(output) => render_spawn_agent_output(output),
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

    if let Some(run_in_background) = obj.get("run_in_background") {
        match run_in_background.as_bool() {
            Some(false) => {
                obj.remove("run_in_background");
            }
            Some(true) => {
                return Err(
                    "unsupported `run_in_background: true` for `agent(action='spawn')`. Backgrounding is a user-controlled UI action: omit this field and let the user press Ctrl+B while the live agent is running."
                        .to_string(),
                );
            }
            None => {
                return Err(
                    "invalid `run_in_background` field for `agent(action='spawn')`: expected boolean false or omit the field."
                        .to_string(),
                );
            }
        }
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
            return render_agent_runtime_binding_error("agent", "get_result");
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
    fn spawn_arg_normalization_rejects_run_in_background_true() {
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

    #[test]
    fn spawn_arg_normalization_ignores_run_in_background_false() {
        let normalized = normalize_agent_spawn_args(&json!({
            "action": "spawn",
            "description": "Review one",
            "prompt": "p1",
            "run_in_background": false,
            "_tool_call_id": "call-1"
        }))
        .expect("false is the synchronous default and should be accepted");
        assert!(normalized.get("run_in_background").is_none());
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
    async fn handle_spawn_agent_tool_floors_too_small_turn_budget_to_agent_default() {
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

        assert!(result.contains("\"status\":\"completed\""), "{result}");
        assert_eq!(
            executor.take_captured_max_turns(),
            Some(60),
            "general-purpose children must not inherit a model-supplied 10-turn cap"
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
        let value: Value = serde_json::from_str(&result).unwrap();
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
        assert_eq!(
            executor.take_captured_model().as_deref(),
            Some("MiniMax-M2.7")
        );
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

    #[test]
    fn fanout_slot_spawn_args_raise_too_small_deep_review_budget() {
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
        assert_eq!(args["max_turns"], 30);
    }

    #[test]
    fn fanout_slot_spawn_args_floor_general_purpose_budget_to_agent_default() {
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
        assert_eq!(args["max_turns"], 60);
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
        let start_value: Value = serde_json::from_str(&start).unwrap();
        assert_eq!(start_value["status"], "completed");
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
    async fn recover_agent_fanout_start_refuses_ambiguous_parent_groups() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor.clone());
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        for group_id in ["review-one", "review-two"] {
            let start = handle_agent_fanout_tool(
                &json!({
                    "action": "start",
                    "group_id": group_id,
                    "target_count": 1,
                    "slots": [
                        {"description": format!("Review {group_id}"), "prompt": "Review changes"}
                    ]
                }),
                Some(&ctx),
            )
            .await;
            let value: Value = serde_json::from_str(&start).unwrap();
            assert_eq!(value["status"], "completed");
        }
        assert_eq!(executor.spawn_count(), 2);

        let recovered = recover_agent_fanout_tool_result(
            &json!({
                "action": "start",
                "target_count": 1,
                "slots": [
                    {"description": "Review unknown", "prompt": "Review changes"}
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
                .is_some_and(|text| text.contains("multiple fanout groups")),
            "{recovered}"
        );
        assert_eq!(
            executor.spawn_count(),
            2,
            "ambiguous recovery must not guess by spawning a replacement group"
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
        let value: Value = serde_json::from_str(&start).unwrap();
        assert_eq!(value["status"], "completed");
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
    async fn agent_fanout_get_results_preserves_budget_adjustment_notice() {
        let spawner = test_spawner(Arc::new(CapturingModelExecutor::new()));
        let ctx = test_spawn_context(spawner.clone(), Some("MiniMax-M2.7"));
        let start = handle_agent_fanout_tool(
            &json!({
                "action": "start",
                "group_id": "review-budget",
                "target_count": 1,
                "defaults": {
                    "agent_type": "code-review",
                    "max_turns": 15,
                    "complexity": "deep"
                },
                "slots": [
                    {"id": "storage", "description": "Review storage", "prompt": "Review storage changes"}
                ]
            }),
            Some(&ctx),
        )
        .await;
        let start_value: Value = serde_json::from_str(&start).unwrap();
        assert_eq!(start_value["status"], "completed");
        assert!(
            start_value["budget_adjustment"]
                .as_str()
                .is_some_and(|notice| notice.contains("max_turns 15")),
            "start response must expose budget adjustment: {start}"
        );

        let result = handle_agent_fanout_tool(
            &json!({
                "action": "get_results",
                "group_id": "review-budget"
            }),
            Some(&ctx),
        )
        .await;
        let value: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(value["status"], "completed");
        assert!(
            value["budget_adjustment"]
                .as_str()
                .is_some_and(|notice| notice.contains("max_turns 15")),
            "get_results response must preserve budget adjustment: {result}"
        );

        let groups = spawner.list_fanout_groups().await;
        assert_eq!(
            groups[0].budget_adjustment.as_deref(),
            value["budget_adjustment"].as_str()
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
        assert!(start.contains("\"status\":\"failed_to_start\""), "{start}");
        assert!(start.contains("\"spawn_rejected\":1"), "{start}");

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
        assert!(start.contains("\"status\":\"failed\""), "{start}");

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
        let start_value: Value = serde_json::from_str(&start).unwrap();
        assert_eq!(start_value["status"], "completed");

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
        let value: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(value["status"], "interrupted");
        assert_eq!(value["cancelled_by_parent_budget"], 1);
        assert!(value.get("cancelled_by_user").is_none(), "{value}");
        assert_eq!(value["agents"][0]["finish_reason"], "budget_exhausted");
        assert!(
            value["interruption_causes"]
                .as_array()
                .is_some_and(|causes| causes.iter().any(|cause| cause == "parent_budget")),
            "{value}"
        );

        let collected = handle_agent_fanout_tool(
            &json!({
                "action": "get_results",
                "group_id": value["group_id"].as_str().unwrap()
            }),
            Some(&ctx),
        )
        .await;
        let collected_value: Value = serde_json::from_str(&collected).unwrap();
        assert_eq!(collected_value["status"], "completed_with_issues");
        assert_eq!(collected_value["cancelled_by_parent_budget"], 1);
        assert_eq!(
            collected_value["results"][0]["result"]["status"],
            "interrupted"
        );
        assert_eq!(collected_value["incomplete_results"], 1);
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
    async fn agent_fanout_empty_args_returns_executable_canonical_shapes() {
        let result = handle_agent_fanout_tool(&json!({}), None).await;
        let value: Value = serde_json::from_str(&result).expect("fanout error must stay JSON");
        assert_eq!(
            value["error_kind"].as_str(),
            Some(astra_core::ErrorKind::ToolInvalidArgs.as_str())
        );
        assert!(
            result.contains("missing required parameter `action` for `agent_fanout`"),
            "{result}"
        );
        assert!(result.contains("Do not retry with empty args"), "{result}");
        // Error recovery must surface all three executable shapes, not just
        // "missing action" — otherwise the model is left to guess the form.
        assert!(result.contains("action='start'"), "{result}");
        assert!(result.contains("action='get_results'"), "{result}");
        assert!(result.contains("action='stop_slot'"), "{result}");
        assert!(result.contains("target_count=N"), "{result}");
        assert!(result.contains("slots=["), "{result}");
        assert!(result.contains("group_id="), "{result}");
        assert!(result.contains("slot_index="), "{result}");
        assert!(result.contains("\"status\":\"failed\""), "{result}");
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
        let value: Value = serde_json::from_str(&result).unwrap();

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
