//! CLI bridge for the shared dynamic-agent tool handler.
//!
//! The `agent(action='spawn'|'get_result'|'send_message')` protocol lives in
//! `astra_runtime::orchestration::agent_tool`. Keep this module thin so CLI
//! and Web/server cannot drift in parsing, normalization, or result rendering.

use serde_json::Value;

pub use astra_runtime::orchestration::AgentToolContext as AgentActionContext;

/// Handle `agent(action='spawn')` using the shared runtime contract.
pub async fn handle_agent_spawn_action(args: &Value, ctx: Option<&AgentActionContext>) -> String {
    astra_runtime::orchestration::handle_agent_spawn_action(args, ctx).await
}

/// Handle `agent(action='get_result')` using the shared runtime contract.
pub async fn handle_agent_get_result_action(
    args: &Value,
    ctx: Option<&AgentActionContext>,
) -> String {
    astra_runtime::orchestration::handle_agent_get_result_action(args, ctx).await
}

/// Handle `agent(action='send_message')` using the same mailbox contract as
/// the server tool surface.
pub async fn handle_agent_send_message_action(
    args: &Value,
    ctx: Option<&AgentActionContext>,
    mailbox_ctx: Option<&super::agent_messaging::SendMessageRuntimeContext>,
) -> String {
    if let Some(ctx) = ctx {
        return astra_runtime::orchestration::handle_agent_tool(args, Some(ctx)).await;
    }
    let Some(mailbox_ctx) = mailbox_ctx else {
        return astra_runtime::orchestration::render_agent_runtime_binding_error(
            "agent",
            "send_message",
        );
    };
    astra_runtime::orchestration::handle_agent_send_message_with_router(
        args,
        mailbox_ctx.router.as_ref(),
        &mailbox_ctx.run_id,
        &mailbox_ctx.agent_id,
    )
    .await
}

/// Handle `agent_fanout(...)` using the shared runtime contract.
pub async fn handle_agent_fanout_tool(args: &Value, ctx: Option<&AgentActionContext>) -> String {
    astra_runtime::orchestration::handle_agent_fanout_tool(args, ctx).await
}

#[cfg(test)]
mod tests {
    use super::handle_agent_spawn_action;
    use serde_json::json;

    #[tokio::test]
    async fn wrapper_uses_shared_no_context_error() {
        let result = handle_agent_spawn_action(
            &json!({
                "description": "Test",
                "prompt": "Test prompt"
            }),
            None,
        )
        .await;

        assert!(
            result.contains("multi-agent runtime is not connected"),
            "{result}"
        );
        assert!(result.contains("\"status\":\"failed\""), "{result}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&result).unwrap()["error_kind"].as_str(),
            Some(astra_core::ErrorKind::ToolBinding.as_str())
        );
    }
}
