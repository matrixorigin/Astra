use serde_json::{Map, Value};

use astra_turn_core::sse_stream_host::EdgeToolExecResult;

use super::host::AgenticLoopState;

pub(crate) fn edge_tool_status_exit_code(status: &str) -> Option<i32> {
    match status.trim().to_ascii_lowercase().as_str() {
        "completed" | "skipped" => Some(0),
        "failed" | "partial_failure" | "denied" | "rejected" | "cancelled" | "timeout"
        | "timed_out" => Some(1),
        _ => None,
    }
}

fn structured_edge_exit_code(fields: Option<&Map<String, Value>>) -> Option<i32> {
    let fields = fields?;
    if let Some(semantics) = fields
        .get("exit_semantics")
        .and_then(Value::as_str)
        .and_then(|tag| {
            serde_json::from_value::<astra_tools::exit_semantics::ExitSemantics>(Value::String(
                tag.to_string(),
            ))
            .ok()
        })
    {
        return Some(if semantics.is_tool_error() { 1 } else { 0 });
    }
    if let Some(result_class) = fields
        .get("result_class")
        .and_then(Value::as_str)
        .and_then(|tag| {
            serde_json::from_value::<astra_tools::exit_semantics::CommandResultClass>(
                Value::String(tag.to_string()),
            )
            .ok()
        })
    {
        return Some(if result_class.is_tool_error() { 1 } else { 0 });
    }
    None
}

fn edge_tool_observability_exit_code(edge_result: &EdgeToolExecResult) -> Option<i32> {
    structured_edge_exit_code(edge_result.tool_result_fields.as_ref())
        .or_else(|| edge_tool_status_exit_code(&edge_result.status))
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
                    edge_tool_observability_exit_code(edge_result),
                );
        }
    }

    if let Some(hub) = &state.telemetry.observability_hub {
        let user_id = state
            .telemetry
            .observability_session
            .as_ref()
            .map(|s| {
                astra_core::sync_poison::recover_rwlock_read(s)
                    .user_id
                    .clone()
            })
            .unwrap_or_default();
        for edge_result in edge_tool_round {
            crate::observability::on_tool_executed(hub, &user_id, &edge_result.tool);
        }
    }
}

/// Generate the OpenAI-compatible tool schema for the "delegate" tool.
#[cfg(test)]
pub(crate) fn delegate_tool_schema() -> Value {
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
                        "enum": ["sequential", "fan_out", "pipeline", "adversarial", "fork", "auto"],
                        "description": "Explicit coordination topology. Omit it to use typed runtime scenario/history signals; task prose is never keyword-classified."
                    },
                    "tasks": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 2,
                        "description": "Explicit sub-task list required by the fork pattern."
                    },
                    "needs_review": {
                        "type": "boolean",
                        "description": "Typed hint for auto/default selection; true may choose adversarial review when exactly two agents are supplied."
                    },
                    "has_dependencies": {
                        "type": "boolean",
                        "description": "Typed hint for auto/default selection; true keeps agents ordered."
                    },
                    "max_rounds": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum rounds for adversarial pattern (default: 2)."
                    },
                    "max_turns": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum turns for each explicit fork task (default: 10)."
                    },
                    "timeout": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Per-agent timeout in seconds; zero disables the timeout."
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
        assert_eq!(edge_tool_status_exit_code("completed"), Some(0));
        assert_eq!(edge_tool_status_exit_code("skipped"), Some(0));
        assert_eq!(edge_tool_status_exit_code("failed"), Some(1));
        assert_eq!(edge_tool_status_exit_code("partial_failure"), Some(1));
        assert_eq!(edge_tool_status_exit_code("rejected"), Some(1));
        assert_eq!(edge_tool_status_exit_code("ok"), None);
        assert_eq!(edge_tool_status_exit_code("success"), None);
        assert_eq!(edge_tool_status_exit_code("error"), None);
        assert_eq!(edge_tool_status_exit_code("unknown"), None);
    }

    #[test]
    fn edge_tool_observability_exit_code_uses_structured_exit_semantics() {
        let result = EdgeToolExecResult {
            request_id: "call-1".into(),
            tool: "bash".into(),
            args: json!({"command": "grep needle haystack.txt"}),
            output: "No matches found".into(),
            tool_result_fields: Some(serde_json::Map::from_iter([
                ("exit_semantics".to_string(), json!("empty_result")),
                ("result_class".to_string(), json!("empty_result")),
            ])),
            status: "failed".into(),
            duration_ms: 10,
        };

        assert_eq!(edge_tool_observability_exit_code(&result), Some(0));
    }

    #[test]
    fn edge_tool_observability_exit_code_structured_error_overrides_status() {
        let result = EdgeToolExecResult {
            request_id: "call-1".into(),
            tool: "bash".into(),
            args: json!({"command": "exit 7"}),
            output: "Error: command failed (exit code 7)".into(),
            tool_result_fields: Some(serde_json::Map::from_iter([
                ("exit_semantics".to_string(), json!("execution_error")),
                ("result_class".to_string(), json!("execution_error")),
            ])),
            status: "completed".into(),
            duration_ms: 10,
        };

        assert_eq!(edge_tool_observability_exit_code(&result), Some(1));
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
        assert!(props["tasks"].is_object());
        assert!(props["needs_review"].is_object());
        assert!(props["has_dependencies"].is_object());
        assert!(props["max_rounds"].is_object());
        assert!(props["max_turns"].is_object());
        assert!(props["timeout"].is_object());
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
        assert!(props.get("tasks").is_some());
        assert!(props.get("needs_review").is_some());
        assert!(props.get("has_dependencies").is_some());
        assert!(props.get("max_rounds").is_some());
        assert!(props.get("max_turns").is_some());
        assert!(props.get("timeout").is_some());
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
