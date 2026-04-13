use serde_json::Value;

use crate::turn::sse_stream_host::EdgeToolExecResult;

use super::agentic_loop_host::AgenticLoopState;

pub(crate) fn edge_tool_status_exit_code(status: &str) -> Option<i32> {
    match status.trim().to_ascii_lowercase().as_str() {
        "ok" | "success" | "succeeded" | "completed" | "complete" | "passed" => Some(0),
        "error" | "failed" | "failure" | "partial_failure" | "denied" | "cancelled"
        | "canceled" | "timeout" | "timed_out" => Some(1),
        _ => None,
    }
}

pub(crate) fn record_edge_tool_observability(
    state: &mut AgenticLoopState,
    edge_tool_round: &[EdgeToolExecResult],
) {
    if let Some(session) = &state.telemetry.observability_session {
        for edge_result in edge_tool_round {
            session
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .record_tool_result(
                    &edge_result.tool,
                    &edge_result.output,
                    edge_tool_status_exit_code(&edge_result.status),
                );
        }
    }

    if let Some(hub) = &state.telemetry.observability_hub {
        let user_id = state
            .telemetry
            .observability_session
            .as_ref()
            .map(|s| s.read().unwrap_or_else(|e| e.into_inner()).user_id.clone())
            .unwrap_or_default();
        for edge_result in edge_tool_round {
            crate::observability_integration::on_tool_executed(hub, &user_id, &edge_result.tool);
        }
    }
}

/// Generate the OpenAI-compatible tool schema for the "delegate" tool.
pub fn delegate_tool_schema() -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "delegate",
            "description": "Delegate a task to one or more specialized sub-agents. Use this when a task benefits from parallel execution, pipeline processing, or adversarial review by specialized agents.",
            "parameters": {
                "type": "object",
                "required": ["task", "agents"],
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "The task description/prompt for the delegated agents."
                    },
                    "agents": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Agent IDs to delegate to. Available: 'coder' (code tasks), 'reviewer' (code review), 'writer' (documentation)."
                    },
                    "pattern": {
                        "type": "string",
                        "enum": ["sequential", "fan_out", "pipeline", "adversarial"],
                        "description": "Coordination pattern. 'sequential': agents run one by one. 'fan_out': agents run in parallel. 'pipeline': output of each feeds the next. 'adversarial': producer+reviewer iterate."
                    },
                    "max_rounds": {
                        "type": "integer",
                        "description": "Maximum rounds for adversarial pattern (default: 2)."
                    },
                    "context": {
                        "type": "object",
                        "description": "Additional context to pass to sub-agents."
                    }
                }
            }
        }
    })
}

/// Extract a file path from an edge tool's name + arguments.
///
/// Covers the common file-touching tools: read_file, write_file, str_replace,
/// grep, glob, find_definition, etc. Returns `None` for non-file tools.
pub(crate) fn extract_file_path_from_tool(tool_name: &str, args: &Value) -> Option<String> {
    match tool_name {
        "read_file" | "write_file" | "str_replace" | "find_definition" => args
            .get("path")
            .or_else(|| args.get("file_path"))
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
        "grep" | "glob" | "list_dir" => args
            .get("path")
            .or_else(|| args.get("directory"))
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
        _ => None,
    }
}
