use serde_json::{Value, json};

use super::tool_execution_binding::{
    ExecutorBindingKind, ExecutorStatus, ToolExecutionRequest, WorkspaceBindingKind,
};
use super::tool_route_selection::ToolExecutionRouteKind;
use super::tool_transport_metadata::{
    RUN_BLOCKED_REASON_EXECUTOR_OFFLINE, RUN_BLOCKED_REASON_ROUTE_MISMATCH,
    TOOL_ERROR_KIND_CAPABILITY_DENIED, attach_runtime_error_metadata,
    attach_runtime_policy_metadata, binding_event_fields,
};

pub(crate) fn edge_unavailable_message(request: &ToolExecutionRequest) -> String {
    let fallback = "No alternate execution provider is available for this file environment.";
    format!(
        "Error: executor '{}' is offline or unreachable for tool '{}'. {}",
        request.executor.display_name, request.tool_name, fallback
    )
}

fn capability_denied_message(
    request: &ToolExecutionRequest,
    reason: &astra_runtime_env::ToolUnavailableReason,
) -> String {
    format!(
        "Error: tool '{}' is not available for this run binding: {}. Select a workspace, executor, runtime, or policy that provides the required capability; no alternate execution provider was attempted.",
        request.tool_name, reason
    )
}

pub(crate) fn capability_denied_result(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    reason: astra_runtime_env::ToolUnavailableReason,
) -> astra_tools::ToolResult {
    if matches!(
        reason,
        astra_runtime_env::ToolUnavailableReason::UnknownTool
    ) {
        return astra_tools::ToolResult {
            output: json!({
                "status": "failed",
                "error": format!(
                    "Unknown tool `{}`. Use only tools advertised in the current turn surface; do not retry this exact name unless it appears in the tool schema.",
                    request.tool_name
                ),
                "error_kind": astra_core::ErrorKind::ToolNotFound.as_str(),
                "retryable": false,
            })
            .to_string(),
            metadata: None,
            is_error: true,
            exit_semantics: None,
        };
    }
    let offline_edge_executor =
        matches!(request.workspace.kind, WorkspaceBindingKind::EdgeWorkspace)
            && matches!(request.executor.kind, ExecutorBindingKind::EdgeAgent)
            && matches!(
                request.executor.status,
                ExecutorStatus::Offline | ExecutorStatus::Unknown
            )
            && !matches!(
                reason,
                astra_runtime_env::ToolUnavailableReason::PolicyDenied(_)
            );
    let mut metadata = binding_event_fields(&request.workspace, &request.executor);
    attach_runtime_policy_metadata(&mut metadata, binding);
    let runtime_error = if offline_edge_executor {
        astra_runtime_env::RuntimeError::executor_offline(edge_unavailable_message(request))
    } else {
        astra_runtime_env::RuntimeError::capability_denied(&request.tool_name, reason.clone())
    };
    let runtime_reason = if offline_edge_executor {
        RUN_BLOCKED_REASON_EXECUTOR_OFFLINE
    } else {
        TOOL_ERROR_KIND_CAPABILITY_DENIED
    };
    attach_runtime_error_metadata(&mut metadata, &runtime_error, runtime_reason);
    metadata.insert(
        "capability_denial".to_string(),
        serde_json::to_value(&reason).unwrap_or(Value::String(reason.to_string())),
    );
    metadata.insert(
        "runtime_environment".to_string(),
        serde_json::to_value(binding).unwrap_or(Value::Null),
    );
    astra_tools::ToolResult {
        output: if offline_edge_executor {
            edge_unavailable_message(request)
        } else {
            capability_denied_message(request, &reason)
        },
        metadata: Some(metadata),
        is_error: true,
        exit_semantics: Some(astra_tools::exit_semantics::ExitSemantics::ExecutionError),
    }
}

fn unsupported_workspace_executor_message(request: &ToolExecutionRequest) -> String {
    let workspace_kind = serde_json::to_value(request.workspace.kind)
        .ok()
        .and_then(|value| value.as_str().map(ToString::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    let executor_kind = serde_json::to_value(request.executor.kind)
        .ok()
        .and_then(|value| value.as_str().map(ToString::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    format!(
        "Error: workspace '{}' ({workspace_kind}) is not routed to an available executor transport for tool '{}'. Bound executor is '{}' ({executor_kind}). Select a workspace provider with an available executor for this tool, then retry. No alternate execution provider was attempted.",
        request.workspace.display_name, request.tool_name, request.executor.display_name
    )
}

pub(crate) fn unsupported_workspace_executor_result(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
) -> astra_tools::ToolResult {
    if !astra_runtime_env::is_mcp_namespaced_tool_name(&request.tool_name)
        && astra_runtime_env::ToolRegistry::builtins()
            .get(&request.tool_name)
            .is_none()
    {
        return astra_tools::ToolResult {
            output: json!({
                "status": "failed",
                "error": format!(
                    "Unknown tool `{}`. Use only tools advertised in the current turn surface; do not retry this exact name unless it appears in the tool schema.",
                    request.tool_name
                ),
                "error_kind": astra_core::ErrorKind::ToolNotFound.as_str(),
                "retryable": false,
            })
            .to_string(),
            metadata: None,
            is_error: true,
            exit_semantics: None,
        };
    }
    let mut blocked_executor = request.executor.clone();
    blocked_executor.status = ExecutorStatus::Degraded;
    let mut metadata = binding_event_fields(&request.workspace, &blocked_executor);
    attach_runtime_policy_metadata(&mut metadata, binding);
    attach_runtime_error_metadata(
        &mut metadata,
        &astra_runtime_env::RuntimeError::route_mismatch(unsupported_workspace_executor_message(
            request,
        )),
        RUN_BLOCKED_REASON_ROUTE_MISMATCH,
    );
    astra_tools::ToolResult {
        output: unsupported_workspace_executor_message(request),
        metadata: Some(metadata),
        is_error: true,
        exit_semantics: Some(astra_tools::exit_semantics::ExitSemantics::ExecutionError),
    }
}

pub(crate) fn selected_offer_route_mismatch_result(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    actual_route: ToolExecutionRouteKind,
) -> astra_tools::ToolResult {
    let selected = request
        .selected_offer
        .as_ref()
        .expect("selected offer route mismatch requires a selected offer");
    let message = format!(
        "Error: selected tool offer '{}' requires route '{}' but this request resolved to route '{}'. Refusing to run the tool through a different provider.",
        selected.offer_id,
        selected.route.as_str(),
        actual_route.as_str()
    );
    let mut degraded_executor = request.executor.clone();
    degraded_executor.status = ExecutorStatus::Degraded;
    let mut metadata = binding_event_fields(&request.workspace, &degraded_executor);
    attach_runtime_policy_metadata(&mut metadata, binding);
    attach_runtime_error_metadata(
        &mut metadata,
        &astra_runtime_env::RuntimeError::route_mismatch(message.clone()),
        RUN_BLOCKED_REASON_ROUTE_MISMATCH,
    );
    metadata.insert(
        "selected_tool_offer".to_string(),
        json!({
            "offer_id": selected.offer_id,
            "provider_id": selected.provider_id,
            "route": selected.route.as_str(),
        }),
    );
    metadata.insert("actual_route".to_string(), json!(actual_route.as_str()));
    astra_tools::ToolResult {
        output: message,
        metadata: Some(metadata),
        is_error: true,
        exit_semantics: Some(astra_tools::exit_semantics::ExitSemantics::ExecutionError),
    }
}

pub(crate) fn transport_adapter_unavailable_result(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    adapter_name: &str,
    diagnostic: &str,
) -> astra_tools::ToolResult {
    let mut degraded_executor = request.executor.clone();
    degraded_executor.status = ExecutorStatus::Degraded;
    let mut metadata = binding_event_fields(&request.workspace, &degraded_executor);
    attach_runtime_policy_metadata(&mut metadata, binding);
    let message = format!(
        "{adapter_name} transport adapter unavailable for tool '{}' on executor '{}': {diagnostic}",
        request.tool_name, request.executor.display_name
    );
    attach_runtime_error_metadata(
        &mut metadata,
        &astra_runtime_env::RuntimeError::transport_unavailable(message.clone()),
        &astra_runtime_env::RuntimeErrorKind::TransportUnavailable.to_string(),
    );
    metadata.insert(
        "diagnostics".to_string(),
        Value::Array(vec![Value::String(diagnostic.to_string())]),
    );
    astra_tools::ToolResult {
        output: format!("Error: {message}. No alternate execution provider was attempted."),
        metadata: Some(metadata),
        is_error: true,
        exit_semantics: Some(astra_tools::exit_semantics::ExitSemantics::ExecutionError),
    }
}
