//! Agent spawning tool for dynamic agent creation.
//!
//! Provides `spawn_agent` tool for creating sub-agents at runtime without
//! pre-defined team configurations.
//!
//! Features:
//! - Built-in agent types (explore, code-review, task, general-purpose)
//! - Background (async) and foreground (sync) execution
//! - Progress event broadcasting
//! - Integration with existing DelegationEngine
//!
//! # Example
//!
//! ```json
//! {
//!   "description": "Search codebase for auth",
//!   "prompt": "Find all authentication-related code in the project",
//!   "agent_type": "explore",
//!   "background": true
//! }
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use serde_json::{Value, json};

use astra_runtime::messaging::router::AgentMailboxRouter;
use astra_runtime::orchestration::{
    DynamicAgentSpawner, SpawnAgentInput, SpawnContext,
    spawn_agent_schema,
};

// ─── Tool Execution Context ────────────────────────────────────────────────

/// Context for spawn_agent tool execution.
pub struct SpawnAgentContext {
    /// Current agent's run ID
    pub run_id: String,
    /// Current agent's ID
    pub agent_id: String,
    /// Working directory
    pub working_dir: PathBuf,
    /// The spawner instance
    pub spawner: Arc<DynamicAgentSpawner>,
}

// ─── Tool Handler ──────────────────────────────────────────────────────────

/// Handle spawn_agent tool call from agentic loop.
///
/// This is called by the tool executor when the LLM invokes spawn_agent.
pub async fn handle_spawn_agent_tool(
    args: &Value,
    ctx: Option<&SpawnAgentContext>,
) -> String {
    // Parse input
    let input: SpawnAgentInput = match serde_json::from_value(args.clone()) {
        Ok(i) => i,
        Err(e) => {
            return json!({
                "status": "failed",
                "error": format!("Invalid input: {}", e)
            }).to_string();
        }
    };

    // Check if spawner is available
    let ctx = match ctx {
        Some(c) => c,
        None => {
            return json!({
                "status": "failed",
                "error": "Agent spawning not available in this context."
            }).to_string();
        }
    };

    // Build spawn context
    let spawn_ctx = SpawnContext {
        parent_run_id: ctx.run_id.clone(),
        parent_agent_id: ctx.agent_id.clone(),
        working_dir: ctx.working_dir.clone(),
    };

    // Execute spawn
    match ctx.spawner.spawn(input, &spawn_ctx).await {
        Ok(output) => {
            serde_json::to_string(&output).unwrap_or_else(|_| {
                json!({
                    "status": "failed",
                    "error": "Failed to serialize output"
                }).to_string()
            })
        }
        Err(e) => {
            json!({
                "status": "failed",
                "error": e.to_string()
            }).to_string()
        }
    }
}

// ─── Tool Schema ───────────────────────────────────────────────────────────

/// Generate the JSON schema for spawn_agent tool.
///
/// Re-exports from astra_runtime::orchestration for convenience.
pub fn get_spawn_agent_schema() -> Value {
    spawn_agent_schema()
}

// ─── Helper Functions ──────────────────────────────────────────────────────

/// Create a SpawnAgentContext from commonly available components.
pub fn create_spawn_context(
    run_id: String,
    agent_id: String,
    working_dir: PathBuf,
    router: Arc<AgentMailboxRouter>,
) -> SpawnAgentContext {
    let spawner = Arc::new(DynamicAgentSpawner::new(router));
    SpawnAgentContext {
        run_id,
        agent_id,
        working_dir,
        spawner,
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_structure() {
        let schema = get_spawn_agent_schema();
        // Schema uses standard format: { type: "function", function: { name, parameters } }
        assert_eq!(schema["type"], "function");
        assert_eq!(schema["function"]["name"], "spawn_agent");
        assert!(schema["function"]["parameters"]["properties"]["description"].is_object());
        assert!(schema["function"]["parameters"]["properties"]["prompt"].is_object());
    }

    #[tokio::test]
    async fn test_handle_invalid_input() {
        let args = json!({"invalid": "data"});
        let result = handle_spawn_agent_tool(&args, None).await;
        assert!(result.contains("Invalid input"));
    }

    #[tokio::test]
    async fn test_handle_no_context() {
        let args = json!({
            "description": "Test",
            "prompt": "Test prompt"
        });
        let result = handle_spawn_agent_tool(&args, None).await;
        assert!(result.contains("not available"));
    }
}
