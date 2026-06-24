use serde_json::Value;

use astra_tools::ToolExecutor;
use astra_tools::executor::DefaultToolExecutor;

use crate::orchestration::AgentToolContext;
use crate::server::tool_execution_result::{
    agent_tool_result_from_output, tool_result_from_output,
};

pub(crate) async fn execute_agent_tool(
    default_executor: &DefaultToolExecutor,
    agent_tool_context: Option<&AgentToolContext>,
    args: &Value,
) -> astra_tools::ToolResult {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    if agent_tool_context.is_none()
        && !astra_turn_core::tool::registry::meta::tool_allows_validation_without_runtime_binding(
            "agent",
            Some(action),
        )
    {
        return agent_tool_result_from_output(
            crate::orchestration::render_agent_runtime_binding_error("agent", action),
        );
    }
    match action {
        "run_chain" => default_executor.execute("run_chain", args).await,
        "spawn" | "get_result" | "send_message" => agent_tool_result_from_output(
            crate::orchestration::handle_agent_tool(args, agent_tool_context).await,
        ),
        other if other.is_empty() && args.get("spawn").is_some() => agent_tool_result_from_output(
            crate::orchestration::handle_agent_tool(args, agent_tool_context).await,
        ),
        other if other.is_empty() && args.get("agents").is_some() => agent_tool_result_from_output(
            crate::orchestration::handle_agent_tool(args, agent_tool_context).await,
        ),
        other => tool_result_from_output(format!(
            "Unknown agent action: '{other}'. Use: spawn, get_result, run_chain."
        )),
    }
}

pub(crate) async fn execute_agent_fanout_tool(
    agent_tool_context: Option<&AgentToolContext>,
    args: &Value,
) -> astra_tools::ToolResult {
    if agent_tool_context.is_none() {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("");
        if !action.is_empty() {
            return agent_tool_result_from_output(
                crate::orchestration::render_agent_runtime_binding_error("agent_fanout", action),
            );
        }
    }
    agent_tool_result_from_output(
        crate::orchestration::handle_agent_fanout_tool(args, agent_tool_context).await,
    )
}
