use serde_json::{Map, Value};

use super::tool_transport_metadata::{
    TOOL_ERROR_KIND_AGENT_WAITING, TOOL_ERROR_KIND_APPROVAL_TIMEOUT, TOOL_ERROR_KIND_CANCELLED,
    TOOL_ERROR_KIND_EXECUTOR_OFFLINE, TOOL_ERROR_KIND_TOOL_TIMEOUT,
    TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED, TOOL_ERROR_KIND_WORKSPACE_PATH_MISMATCH,
};

pub(crate) fn tool_result_from_output(output: String) -> astra_tools::ToolResult {
    let parsed = serde_json::from_str::<Value>(&output).ok();
    let json_error = parsed
        .as_ref()
        .and_then(|value| value.get("success").and_then(Value::as_bool))
        .is_some_and(|success| !success)
        || parsed
            .as_ref()
            .and_then(|value| value.get("error"))
            .is_some();
    let mut result = if output.starts_with("Error:")
        || output.starts_with("SANDBOX_DENIED:")
        || output.starts_with("PATH_RESOLUTION_FAILED:")
        || json_error
    {
        astra_tools::ToolResult::error(output)
    } else {
        astra_tools::ToolResult::text(output)
    };
    if let Some(error_kind) = parsed
        .as_ref()
        .and_then(|value| value.get("error_kind"))
        .and_then(Value::as_str)
    {
        let metadata = result.metadata.get_or_insert_with(Map::new);
        metadata.insert(
            "error_kind".to_string(),
            Value::String(error_kind.to_string()),
        );
    }
    result
}

fn normalized_wait_reason(reason: &str) -> String {
    reason
        .trim()
        .strip_prefix("waiting:")
        .unwrap_or_else(|| reason.trim())
        .trim()
        .to_ascii_lowercase()
}

fn execution_boundary_wait_error_kind(reason: &str) -> Option<&'static str> {
    let normalized = normalized_wait_reason(reason);
    if normalized == TOOL_ERROR_KIND_EXECUTOR_OFFLINE
        || normalized.starts_with(&format!("{TOOL_ERROR_KIND_EXECUTOR_OFFLINE}:"))
    {
        Some(TOOL_ERROR_KIND_EXECUTOR_OFFLINE)
    } else if normalized == TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED
        || normalized.starts_with(&format!("{TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED}:"))
    {
        Some(TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED)
    } else {
        None
    }
}

pub(crate) fn agent_tool_result_from_output(output: String) -> astra_tools::ToolResult {
    let parsed = serde_json::from_str::<Value>(&output).ok();
    let waiting_reason = parsed.as_ref().and_then(|value| {
        let status = value.get("status").and_then(Value::as_str)?;
        if status != "waiting" {
            return None;
        }
        Some(
            value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("waiting"),
        )
    });

    let Some(reason) = waiting_reason else {
        return tool_result_from_output(output);
    };

    let normalized_reason = normalized_wait_reason(reason);
    let error_kind =
        execution_boundary_wait_error_kind(reason).unwrap_or(TOOL_ERROR_KIND_AGENT_WAITING);
    let mut result = astra_tools::ToolResult::error(output);
    let mut metadata = Map::new();
    metadata.insert(
        "error_kind".to_string(),
        Value::String(error_kind.to_string()),
    );
    metadata.insert("reason".to_string(), Value::String(normalized_reason));
    metadata.insert("blocked".to_string(), Value::Bool(true));
    metadata.insert(
        "agent_status".to_string(),
        Value::String("waiting".to_string()),
    );
    if let Some(agent_id) = parsed
        .as_ref()
        .and_then(|value| value.get("agent_id"))
        .and_then(Value::as_str)
    {
        metadata.insert("agent_id".to_string(), Value::String(agent_id.to_string()));
    }
    result.metadata = Some(metadata);
    result
}

pub(crate) fn approval_timeout_tool_result() -> astra_tools::ToolResult {
    let mut result =
        astra_tools::ToolResult::error("Tool execution denied: approval request timed out".into());
    result.metadata = Some(Map::from_iter([
        (
            "error_kind".to_string(),
            Value::String(TOOL_ERROR_KIND_APPROVAL_TIMEOUT.to_string()),
        ),
        (
            "reason".to_string(),
            Value::String(TOOL_ERROR_KIND_APPROVAL_TIMEOUT.to_string()),
        ),
        ("blocked".to_string(), Value::Bool(true)),
    ]));
    result
}

pub(crate) fn tool_timeout_tool_result(message: String) -> astra_tools::ToolResult {
    let mut result = astra_tools::ToolResult::error(message);
    result.metadata = Some(Map::from_iter([
        (
            "error_kind".to_string(),
            Value::String(TOOL_ERROR_KIND_TOOL_TIMEOUT.to_string()),
        ),
        (
            "reason".to_string(),
            Value::String(TOOL_ERROR_KIND_TOOL_TIMEOUT.to_string()),
        ),
    ]));
    result
}

pub(crate) fn annotate_default_executor_cancel_if_needed(
    tool_name: &str,
    result: &mut astra_tools::ToolResult,
) {
    if !result.is_error || result_metadata_str(result, "error_kind").is_some() {
        return;
    }
    let cancelled_before = format!("Tool '{tool_name}' cancelled before completion");
    let not_executed = format!("Tool '{tool_name}' not executed: run was cancelled");
    if result.output != cancelled_before && result.output != not_executed {
        return;
    }
    result.metadata = Some(Map::from_iter([
        (
            "error_kind".to_string(),
            Value::String(TOOL_ERROR_KIND_CANCELLED.to_string()),
        ),
        (
            "reason".to_string(),
            Value::String(TOOL_ERROR_KIND_CANCELLED.to_string()),
        ),
        ("cancelled".to_string(), Value::Bool(true)),
    ]));
}

pub(crate) fn workspace_path_mismatch_tool_result(message: String) -> astra_tools::ToolResult {
    let mut result = astra_tools::ToolResult::error(message);
    result.metadata = Some(Map::from_iter([
        (
            "error_kind".to_string(),
            Value::String(TOOL_ERROR_KIND_WORKSPACE_PATH_MISMATCH.to_string()),
        ),
        (
            "reason".to_string(),
            Value::String(TOOL_ERROR_KIND_WORKSPACE_PATH_MISMATCH.to_string()),
        ),
        ("blocked".to_string(), Value::Bool(true)),
    ]));
    result
}

pub(crate) fn result_metadata_str<'a>(
    result: &'a astra_tools::ToolResult,
    key: &str,
) -> Option<&'a str> {
    result
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get(key))
        .and_then(Value::as_str)
}
