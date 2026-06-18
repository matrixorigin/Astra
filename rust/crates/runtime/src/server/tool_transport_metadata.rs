use serde_json::{json, Map, Value};

use super::tool_execution_binding::{
    ExecutorBinding, ExecutorStatus, ToolExecutionRequest, ToolTransportKind, WorkspaceBinding,
};

pub const TOOL_ERROR_KIND_APPROVAL_TIMEOUT: &str = "approval_timeout";
pub const TOOL_ERROR_KIND_TOOL_TIMEOUT: &str = "tool_timeout";
pub const TOOL_ERROR_KIND_WORKSPACE_PATH_MISMATCH: &str = "workspace_path_mismatch";
pub const TOOL_ERROR_KIND_AGENT_WAITING: &str = "agent_waiting";
pub const TOOL_ERROR_KIND_FALLBACK_DISABLED: &str = "fallback_disabled";
pub const TOOL_ERROR_KIND_EXECUTOR_OFFLINE: &str = "executor_offline";
pub const RUN_BLOCKED_REASON_EXECUTOR_OFFLINE: &str = TOOL_ERROR_KIND_EXECUTOR_OFFLINE;
pub const TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED: &str = "transport_disconnected";
pub const TOOL_ERROR_KIND_CANCELLED: &str = "cancelled";
pub const TOOL_ERROR_KIND_CAPABILITY_DENIED: &str = "capability_denied";
pub const RUN_BLOCKED_REASON_TRANSPORT_DISCONNECTED: &str = TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED;
pub const RUN_BLOCKED_REASON_FALLBACK_DISABLED: &str = TOOL_ERROR_KIND_FALLBACK_DISABLED;
pub const TOOL_ERROR_KIND_ROUTE_MISMATCH: &str = "route_mismatch";
pub const RUN_BLOCKED_REASON_ROUTE_MISMATCH: &str = TOOL_ERROR_KIND_ROUTE_MISMATCH;

pub(crate) fn cancelled_runtime_tool_result(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    transport: ToolTransportKind,
    execution_started: bool,
) -> astra_tools::ToolResult {
    cancelled_runtime_tool_result_for_binding(
        &request.workspace,
        &request.executor,
        &request.tool_name,
        binding,
        transport,
        execution_started,
    )
}

pub(crate) fn cancelled_runtime_tool_result_for_binding(
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    tool_name: &str,
    binding: &astra_runtime_env::RunBinding,
    transport: ToolTransportKind,
    execution_started: bool,
) -> astra_tools::ToolResult {
    let mut metadata = delivered_binding_event_fields(workspace, executor, transport);
    attach_runtime_policy_metadata(&mut metadata, binding);
    metadata.insert("cancelled".to_string(), Value::Bool(true));
    let message = format!("Tool '{tool_name}' cancelled before completion");
    let error = if execution_started {
        astra_runtime_env::RuntimeError::after_start(
            astra_runtime_env::RuntimeErrorKind::Cancelled,
            message.clone(),
        )
    } else {
        astra_runtime_env::RuntimeError::new(
            astra_runtime_env::RuntimeErrorKind::Cancelled,
            message.clone(),
        )
    };
    attach_runtime_error_metadata(&mut metadata, &error, TOOL_ERROR_KIND_CANCELLED);
    astra_tools::ToolResult {
        output: message,
        metadata: Some(metadata),
        is_error: true,
        exit_semantics: Some(astra_tools::exit_semantics::ExitSemantics::ExecutionError),
    }
}

pub(crate) fn runtime_tool_timeout_result(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    transport: ToolTransportKind,
    execution_started: bool,
    max_execution_secs: f64,
) -> astra_tools::ToolResult {
    let mut metadata =
        delivered_binding_event_fields(&request.workspace, &request.executor, transport);
    attach_runtime_policy_metadata(&mut metadata, binding);
    metadata.insert("max_execution_secs".to_string(), json!(max_execution_secs));
    let message = format!(
        "Tool '{}' exceeded max_execution_secs {} before completion",
        request.tool_name,
        format_timeout_seconds(max_execution_secs)
    );
    let error = if execution_started {
        astra_runtime_env::RuntimeError::after_start(
            astra_runtime_env::RuntimeErrorKind::ToolTimeout,
            message.clone(),
        )
    } else {
        astra_runtime_env::RuntimeError::new(
            astra_runtime_env::RuntimeErrorKind::ToolTimeout,
            message.clone(),
        )
    };
    attach_runtime_error_metadata(&mut metadata, &error, TOOL_ERROR_KIND_TOOL_TIMEOUT);
    astra_tools::ToolResult {
        output: format!("Error: {message}"),
        metadata: Some(metadata),
        is_error: true,
        exit_semantics: Some(astra_tools::exit_semantics::ExitSemantics::ExecutionError),
    }
}

pub(crate) fn runtime_execution_timeout_duration(
    binding: &astra_runtime_env::RunBinding,
) -> Option<std::time::Duration> {
    let seconds = binding.policy.resources.max_execution_secs?;
    if !seconds.is_finite() {
        return None;
    }
    Some(std::time::Duration::from_secs_f64(seconds.max(0.0)))
}

fn format_timeout_seconds(seconds: f64) -> String {
    let mut text = format!("{seconds:.3}");
    while text.contains('.') && text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}

pub(crate) fn attach_runtime_policy_metadata(
    metadata: &mut Map<String, Value>,
    binding: &astra_runtime_env::RunBinding,
) {
    metadata
        .entry("runtime".to_string())
        .or_insert_with(|| serde_json::to_value(&binding.runtime).unwrap_or(Value::Null));
    let policy = astra_runtime_env::CompiledRuntimePolicy::initial(binding.policy.clone());
    metadata.entry("policy".to_string()).or_insert_with(|| {
        json!({
            "revision": policy.revision,
            "update_mode": policy.update_mode,
            "intent": policy.intent,
        })
    });
    metadata
        .entry("runtime_environment".to_string())
        .or_insert_with(|| serde_json::to_value(binding).unwrap_or(Value::Null));
}

pub(crate) fn attach_runtime_error_metadata(
    metadata: &mut Map<String, Value>,
    error: &astra_runtime_env::RuntimeError,
    reason: &str,
) {
    metadata.insert(
        "error_kind".to_string(),
        Value::String(error.kind.to_string()),
    );
    metadata.insert("reason".to_string(), Value::String(reason.to_string()));
    metadata.insert("blocked".to_string(), Value::Bool(true));
    metadata.insert("retryable".to_string(), Value::Bool(error.retryable));
    metadata.insert(
        "execution_started".to_string(),
        Value::Bool(error.execution_started),
    );
    metadata.insert(
        "side_effects_maybe".to_string(),
        Value::Bool(error.side_effects_maybe),
    );
    metadata.insert(
        "next_action".to_string(),
        serde_json::to_value(error.next_action).unwrap_or(Value::Null),
    );
    metadata.insert(
        "runtime_error".to_string(),
        serde_json::to_value(error).unwrap_or(Value::Null),
    );
}

pub(crate) fn delivered_binding_event_fields(
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    transport: ToolTransportKind,
) -> Map<String, Value> {
    let mut delivered_executor = executor.clone();
    delivered_executor.transport = transport;
    delivered_executor.status = ExecutorStatus::Online;
    binding_event_fields(workspace, &delivered_executor)
}

pub(crate) fn output_limit_exceeded_result(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    transport: ToolTransportKind,
    outcome: &astra_runtime_env::RuntimeToolOutcome,
) -> Option<astra_tools::ToolResult> {
    let max_output_bytes = binding.policy.resources.max_output_bytes?;
    let output_bytes = outcome.output.len();
    if output_bytes <= max_output_bytes {
        return None;
    }

    let mut metadata = outcome.metadata.clone();
    for (key, value) in
        delivered_binding_event_fields(&request.workspace, &request.executor, transport)
    {
        metadata.entry(key).or_insert(value);
    }
    attach_runtime_policy_metadata(&mut metadata, binding);
    metadata.insert("output_bytes".to_string(), json!(output_bytes));
    metadata.insert("max_output_bytes".to_string(), json!(max_output_bytes));
    let error = astra_runtime_env::RuntimeError::after_start(
        astra_runtime_env::RuntimeErrorKind::OutputLimitExceeded,
        format!(
            "tool '{}' produced {output_bytes} bytes, exceeding max_output_bytes {max_output_bytes}",
            request.tool_name
        ),
    );
    let reason = error.kind.to_string();
    attach_runtime_error_metadata(&mut metadata, &error, &reason);

    Some(astra_tools::ToolResult {
        output: format!(
            "Error: output limit exceeded for tool '{}': output was {output_bytes} bytes, limit is {max_output_bytes} bytes",
            request.tool_name
        ),
        metadata: Some(metadata),
        is_error: true,
        exit_semantics: Some(astra_tools::exit_semantics::ExitSemantics::ExecutionError),
    })
}

pub fn binding_event_fields(
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert(
        "workspace".to_string(),
        serde_json::to_value(workspace).unwrap_or(Value::Null),
    );
    fields.insert(
        "executor".to_string(),
        serde_json::to_value(executor).unwrap_or(Value::Null),
    );
    fields.insert(
        "transport".to_string(),
        serde_json::to_value(executor.transport).unwrap_or(Value::Null),
    );
    fields.insert(
        "fallback_policy".to_string(),
        serde_json::to_value(workspace.fallback_policy).unwrap_or(Value::Null),
    );
    fields
}
