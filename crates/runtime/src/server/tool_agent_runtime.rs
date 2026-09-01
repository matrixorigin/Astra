use serde_json::Value;

use astra_tools::agent_tool_contract::{
    AgentAction, agent_action_from_args, agent_fanout_action_from_args, has_malformed_tool_args,
};
use astra_tools::executor::DefaultToolExecutor;

use crate::orchestration::AgentToolContext;
use crate::server::tool_execution_result::agent_tool_result_from_output;

pub(crate) async fn execute_agent_tool(
    _default_executor: &DefaultToolExecutor,
    agent_tool_context: Option<&AgentToolContext>,
    args: &Value,
    tool_call_id: Option<&str>,
) -> astra_tools::ToolResult {
    let correlated_args = correlated_agent_arguments(args, tool_call_id);
    if has_malformed_tool_args(args) {
        return agent_tool_result_from_output(
            crate::orchestration::handle_agent_tool(&correlated_args, agent_tool_context).await,
        );
    }
    let action = match agent_action_from_args(args) {
        Ok(action) => action,
        Err(error) => return agent_tool_result_from_output(render_agent_error(error)),
    };
    if agent_tool_context.is_none()
        && !astra_turn_core::tool::registry::meta::tool_allows_validation_without_runtime_binding(
            "agent",
            Some(action.as_str()),
        )
    {
        return agent_tool_result_from_output(
            crate::orchestration::render_agent_runtime_binding_error("agent", action.as_str()),
        );
    }
    match action {
        AgentAction::RunChain => server_agent_run_chain_unavailable_result(),
        AgentAction::Spawn | AgentAction::GetResult | AgentAction::SendMessage => {
            agent_tool_result_from_output(
                crate::orchestration::handle_agent_tool(&correlated_args, agent_tool_context).await,
            )
        }
    }
}

fn server_agent_run_chain_unavailable_result() -> astra_tools::ToolResult {
    let mut result = astra_tools::ToolResult::error(
        "The agent.run_chain action is local-executor-only and is not part of the server agent contract. Use start_work for durable task tracking, or call the visible tools directly."
            .to_string(),
    );
    result.metadata = Some(serde_json::Map::from_iter([
        (
            "error_kind".to_string(),
            Value::String("tool_action_not_available".to_string()),
        ),
        ("tool_name".to_string(), Value::String("agent".to_string())),
        ("action".to_string(), Value::String("run_chain".to_string())),
        (
            "available_actions".to_string(),
            serde_json::json!(["spawn", "get_result", "send_message"]),
        ),
    ]));
    result
}

pub(crate) async fn execute_agent_fanout_tool(
    agent_tool_context: Option<&AgentToolContext>,
    args: &Value,
    tool_call_id: Option<&str>,
) -> astra_tools::ToolResult {
    let correlated_args = correlated_agent_arguments(args, tool_call_id);
    if has_malformed_tool_args(args) {
        return agent_tool_result_from_output(
            crate::orchestration::handle_agent_fanout_tool(&correlated_args, agent_tool_context)
                .await,
        );
    }
    let action = match agent_fanout_action_from_args(args) {
        Ok(action) => action,
        Err(error) => return agent_tool_result_from_output(render_agent_error(error)),
    };
    if agent_tool_context.is_none() {
        return agent_tool_result_from_output(
            crate::orchestration::render_agent_runtime_binding_error(
                "agent_fanout",
                action.as_str(),
            ),
        );
    }
    agent_tool_result_from_output(
        crate::orchestration::handle_agent_fanout_tool(&correlated_args, agent_tool_context).await,
    )
}

fn correlated_agent_arguments(args: &Value, tool_call_id: Option<&str>) -> Value {
    let Some(tool_call_id) = tool_call_id.filter(|value| !value.is_empty()) else {
        return args.clone();
    };
    let Some(mut object) = args.as_object().cloned() else {
        return args.clone();
    };
    object.insert(
        "_tool_call_id".to_string(),
        Value::String(tool_call_id.to_string()),
    );
    Value::Object(object)
}

fn render_agent_error(error: String) -> String {
    astra_turn_core::orchestration::agent_result_wire::render_agent_tool_error_with_kind(
        None,
        &error,
        Some(astra_core::ErrorKind::ToolInvalidArgs),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forged_server_run_chain_is_typed_action_unavailable_not_agent_unavailable() {
        let result = server_agent_run_chain_unavailable_result();
        assert!(result.is_error);
        assert!(result.output.contains("local-executor-only"));
        assert!(!result.output.contains("Tool 'run_chain' not available"));
        let metadata = result.metadata.expect("typed metadata");
        assert_eq!(metadata["error_kind"], "tool_action_not_available");
        assert_eq!(metadata["tool_name"], "agent");
        assert_eq!(metadata["action"], "run_chain");
        assert_eq!(
            metadata["available_actions"],
            serde_json::json!(["spawn", "get_result", "send_message"])
        );
    }
}
