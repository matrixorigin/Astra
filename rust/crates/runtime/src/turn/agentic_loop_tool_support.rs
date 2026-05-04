use serde_json::Value;

use astra_turn_core::sse_stream_host::EdgeToolExecResult;

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
            "description": "Delegate a task to specialized sub-agents for parallel, sequential, pipeline, or review workflows.",
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
                        "description": "Coordination pattern: sequential, fan_out, pipeline, or adversarial."
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn edge_tool_status_exit_code_maps_common_statuses() {
        assert_eq!(edge_tool_status_exit_code("ok"), Some(0));
        assert_eq!(edge_tool_status_exit_code("completed"), Some(0));
        assert_eq!(edge_tool_status_exit_code("error"), Some(1));
        assert_eq!(edge_tool_status_exit_code("partial_failure"), Some(1));
        assert_eq!(edge_tool_status_exit_code("unknown"), None);
    }

    #[test]
    fn delegate_tool_schema_has_correct_structure() {
        let schema = delegate_tool_schema();
        assert_eq!(schema["type"], "function");
        assert_eq!(schema["function"]["name"], "delegate");

        let params = &schema["function"]["parameters"];
        assert_eq!(params["type"], "object");

        let required = params["required"].as_array().unwrap();
        assert!(required.contains(&json!("task")));
        assert!(required.contains(&json!("agents")));

        let props = &params["properties"];
        assert!(props["task"].is_object());
        assert!(props["agents"].is_object());
        assert!(props["pattern"].is_object());
        assert!(props["max_rounds"].is_object());
        assert!(props["context"].is_object());
    }

    #[test]
    fn delegate_schema_has_required_openai_structure() {
        let schema = delegate_tool_schema();
        assert_eq!(schema["type"], "function");
        assert_eq!(schema["function"]["name"], "delegate");
        assert!(schema["function"]["description"].as_str().unwrap().len() > 10);
        let params = &schema["function"]["parameters"];
        assert_eq!(params["type"], "object");
        let required = params["required"].as_array().unwrap();
        assert!(required.contains(&json!("task")));
        assert!(required.contains(&json!("agents")));
        let props = &params["properties"];
        assert!(props.get("task").is_some());
        assert!(props.get("agents").is_some());
        assert!(props.get("pattern").is_some());
        assert!(props.get("max_rounds").is_some());
        assert!(props.get("context").is_some());
    }

    #[test]
    fn extract_file_path_from_tool_reads_common_path_fields() {
        assert_eq!(
            extract_file_path_from_tool("read_file", &json!({ "path": "/tmp/a.txt" })),
            Some("/tmp/a.txt".to_string())
        );
        assert_eq!(
            extract_file_path_from_tool("write_file", &json!({ "file_path": "/tmp/b.txt" })),
            Some("/tmp/b.txt".to_string())
        );
        assert_eq!(
            extract_file_path_from_tool("glob", &json!({ "directory": "/tmp/c" })),
            Some("/tmp/c".to_string())
        );
        assert_eq!(
            extract_file_path_from_tool("bash", &json!({ "command": "pwd" })),
            None
        );
    }
}
