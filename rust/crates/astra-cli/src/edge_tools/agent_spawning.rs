//! CLI compatibility wrappers for the shared dynamic-agent tool handler.
//!
//! The `agent(action='spawn'|'get_result')` protocol lives in
//! `astra_runtime::orchestration::agent_tool`. Keep this module thin so CLI
//! and Web/server cannot drift in parsing, normalization, or result rendering.

use serde_json::Value;

pub use astra_runtime::orchestration::AgentToolContext as SpawnAgentContext;

/// Handle `agent(action='spawn')` using the shared runtime contract.
pub async fn handle_spawn_agent_tool(args: &Value, ctx: Option<&SpawnAgentContext>) -> String {
    astra_runtime::orchestration::handle_agent_spawn_tool(args, ctx).await
}

/// Handle `agent(action='get_result')` using the shared runtime contract.
pub async fn handle_get_agent_result_tool(args: &Value, ctx: Option<&SpawnAgentContext>) -> String {
    astra_runtime::orchestration::handle_agent_get_result_tool(args, ctx).await
}

/// JSON schema for the legacy standalone get-agent-result tool.
pub fn get_agent_result_schema() -> Value {
    astra_runtime::orchestration::get_agent_result_schema()
}

/// JSON schema for the legacy standalone spawn-agent tool.
pub fn get_spawn_agent_schema() -> Value {
    astra_runtime::orchestration::get_spawn_agent_schema()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wrapper_exposes_spawn_schema() {
        let schema = get_spawn_agent_schema();
        assert_eq!(schema["type"], "function");
        assert_eq!(schema["function"]["name"], "spawn_agent");
    }

    #[test]
    fn wrapper_exposes_get_result_schema() {
        let schema = get_agent_result_schema();
        assert_eq!(schema["type"], "function");
        assert_eq!(schema["function"]["name"], "get_agent_result");
        assert!(schema["function"]["parameters"]["properties"]["agent_id"].is_object());
    }

    #[tokio::test]
    async fn wrapper_uses_shared_no_context_error() {
        let result = handle_spawn_agent_tool(
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
