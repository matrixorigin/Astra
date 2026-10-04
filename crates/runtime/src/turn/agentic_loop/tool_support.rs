use serde_json::{Map, Value};

use astra_turn_core::sse_stream_host::EdgeToolExecResult;

use super::host::AgenticLoopState;

pub(crate) fn edge_tool_status_exit_code(status: &str) -> Option<i32> {
    astra_thin_client::tool_result_status_is_error(status)
        .map(|is_error| if is_error { 1 } else { 0 })
}

/// Process exit semantics cannot erase a failure of the enclosing tool
/// contract (for example a missing executor verification receipt).
pub(crate) fn has_typed_executor_failure(fields: &Map<String, Value>) -> bool {
    fields
        .get("error_kind")
        .is_some_and(|value| serde_json::from_value::<astra_core::ErrorKind>(value.clone()).is_ok())
        || fields.get("recovery_evidence").is_some_and(|value| {
            serde_json::from_value::<astra_core::ToolFailureEvidence>(value.clone()).is_ok()
        })
}

pub(crate) fn record_edge_tool_observability(
    state: &mut AgenticLoopState,
    edge_tool_round: &[EdgeToolExecResult],
) {
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
        assert_eq!(edge_tool_status_exit_code("success"), None);
        assert_eq!(edge_tool_status_exit_code("ok"), None);
        assert_eq!(edge_tool_status_exit_code("skipped"), None);
        assert_eq!(edge_tool_status_exit_code("failed"), Some(1));
        assert_eq!(edge_tool_status_exit_code("error"), None);
        assert_eq!(edge_tool_status_exit_code("partial_failure"), Some(1));
        assert_eq!(edge_tool_status_exit_code("rejected"), Some(1));
        assert_eq!(edge_tool_status_exit_code("interrupted"), Some(1));
        assert_eq!(edge_tool_status_exit_code("timeout"), Some(1));
        assert_eq!(edge_tool_status_exit_code("unexpected"), None);
        assert_eq!(edge_tool_status_exit_code("unknown"), None);
        assert_eq!(edge_tool_status_exit_code(" OK "), None);
        assert_eq!(edge_tool_status_exit_code("SUCCESS"), None);
    }

    #[test]
    fn edge_tool_observation_records_usage_for_the_bound_profile() {
        use super::super::host::tests::{make_edge_tool, make_state};
        use std::sync::Arc;
        let hub = Arc::new(crate::observability::ObservabilityHub::new());
        let session = hub.start_session("tool-owner", "tool-session");
        let mut state = make_state();
        state.telemetry.observability_hub = Some(hub.clone());
        state.telemetry.observability_session = Some(session);
        let mut failed = make_edge_tool("read_file", "missing");
        failed.status = "failed".into();
        record_edge_tool_observability(
            &mut state,
            &[make_edge_tool("read_file", "contents"), failed],
        );
        let profile = hub.profiles().get_profile("tool-owner");
        assert_eq!(profile.stats.total_tool_calls, 2);
        assert_eq!(profile.stats.tool_usage.get("read_file"), Some(&2));
        assert_eq!(
            hub.profiles()
                .get_profile("unrelated-owner")
                .stats
                .total_tool_calls,
            0
        );
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
