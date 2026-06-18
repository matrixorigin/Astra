use astra_turn_core::cloud::approval_policy::{
    CloudGatedToolKind, cloud_gated_tool_kind_with_args,
};
use serde_json::Value;

/// Argument-aware plan-mode write guard shared by server-local execution and
/// headless permission gating.
///
/// Plan mode is an authoring phase: read-only exploration and plan-control
/// tools stay available, while write/execute-class tools and server-local
/// state mutation tools are denied before normal permission escalation.
pub(crate) fn is_plan_mode_blocked_tool(tool_name: &str, args: &Value) -> bool {
    if tool_name == "task_stop" {
        return true;
    }
    if tool_name == "task" {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("");
        return action == "stop";
    }
    if astra_turn_core::tool::schema::prune::PLAN_MODE_REQUIRED_TOOLS.contains(&tool_name) {
        return false;
    }
    if tool_name == "git" {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("");
        return match action {
            "commit" | "revert_commit" | "push" => true,
            "stash" => args
                .get("stash_action")
                .or_else(|| args.get("sub_action"))
                .and_then(Value::as_str)
                .is_some_and(|stash_action| {
                    matches!(
                        stash_action,
                        "push" | "save" | "apply" | "pop" | "drop" | "branch"
                    )
                }),
            _ => false,
        };
    }
    if tool_name == "github" {
        return args
            .get("action")
            .and_then(Value::as_str)
            .is_some_and(|action| action == "create_issue");
    }
    if is_server_local_mutation_tool(tool_name) {
        return true;
    }
    matches!(
        cloud_gated_tool_kind_with_args(tool_name, Some(args)),
        Some(CloudGatedToolKind::Write | CloudGatedToolKind::Execute)
    )
}

fn is_server_local_mutation_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "delete_file"
            | "multi_edit"
            | "rollback_file_edits"
            | "rollback_session_state"
            | "adjust_config"
            | "prioritize_tool"
            | "deprioritize_tool"
            | "compress_context"
            | "publish_artifact"
            | "run_script"
            | "rollback_database_snapshots"
    )
}
