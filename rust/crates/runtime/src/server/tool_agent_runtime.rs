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
    match action {
        // `agent(action='delegate')` was never intercepted by the real
        // delegation engine, which matches on the standalone tool name.
        "delegate" => astra_tools::ToolResult::error(
            "Error: agent.delegate has been removed because action-shaped \
             delegation was not intercepted by the delegation engine. Use \
             agent(action='spawn', description='...', prompt='...', run_in_background: true) \
             instead."
                .to_string(),
        ),
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
    agent_tool_result_from_output(
        crate::orchestration::handle_agent_fanout_tool(args, agent_tool_context).await,
    )
}
