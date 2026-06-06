//! CLI bridge for the shared dynamic-agent tool handler.
//!
//! The `agent(action='spawn'|'get_result')` protocol lives in
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

        assert!(result.contains("Agent spawning not available"));
        assert!(result.contains("\"status\":\"failed\""), "{result}");
    }
}
