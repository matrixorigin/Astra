use serde_json::Value;

use astra_tools::ToolExecutor;
use astra_tools::agent_tool_contract::{
    AgentAction, agent_action_from_args, agent_fanout_action_from_args,
};
use astra_tools::executor::DefaultToolExecutor;

use crate::orchestration::AgentToolContext;
use crate::server::tool_execution_result::agent_tool_result_from_output;

pub(crate) async fn execute_agent_tool(
    default_executor: &DefaultToolExecutor,
    agent_tool_context: Option<&AgentToolContext>,
    args: &Value,
) -> astra_tools::ToolResult {
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
        AgentAction::RunChain => default_executor.execute("run_chain", args).await,
        AgentAction::Spawn | AgentAction::GetResult | AgentAction::SendMessage => {
            agent_tool_result_from_output(
                crate::orchestration::handle_agent_tool(args, agent_tool_context).await,
            )
        }
    }
}

pub(crate) async fn execute_agent_fanout_tool(
    agent_tool_context: Option<&AgentToolContext>,
    args: &Value,
) -> astra_tools::ToolResult {
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
        crate::orchestration::handle_agent_fanout_tool(args, agent_tool_context).await,
    )
}

fn render_agent_error(error: String) -> String {
    astra_turn_core::orchestration::agent_result_wire::render_agent_tool_error(None, &error)
}
