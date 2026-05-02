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

use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

use astra_runtime::orchestration::{
    DynamicAgentSpawner, InheritedPermissions, SpawnAgentInput, SpawnContext, spawn_agent_schema,
};

// ─── Tool Execution Context ────────────────────────────────────────────────

/// Context for spawn_agent tool execution.
pub struct SpawnAgentContext {
    /// Current agent's run ID
    pub run_id: String,
    /// Current agent's ID
    pub agent_id: String,
    /// Current nested agent/sub-run depth of the agent.
    pub recursion_depth: u8,
    /// Working directory
    pub working_dir: PathBuf,
    /// The spawner instance
    pub spawner: Arc<DynamicAgentSpawner>,
    /// Effective permissions inherited by children spawned from this agent.
    pub inherited_permissions: InheritedPermissions,
    /// Skills available to this agent (inherited by children).
    pub active_skills: Vec<String>,
}

// ─── Tool Handler ──────────────────────────────────────────────────────────

/// Handle spawn_agent tool call from agentic loop.
///
/// This is called by the tool executor when the LLM invokes spawn_agent.
pub async fn handle_spawn_agent_tool(args: &Value, ctx: Option<&SpawnAgentContext>) -> String {
    // Parse input
    let input: SpawnAgentInput = match serde_json::from_value(args.clone()) {
        Ok(i) => i,
        Err(e) => {
            return json!({
                "status": "failed",
                "error": format!("Invalid input: {}", e)
            })
            .to_string();
        }
    };

    // Check if spawner is available
    let ctx = match ctx {
        Some(c) => c,
        None => {
            return json!({
                "status": "failed",
                "error": "Agent spawning not available in this context."
            })
            .to_string();
        }
    };

    // Build spawn context
    let mut inherited_permissions = ctx.inherited_permissions.clone();
    inherited_permissions.is_background = input.background;
    let spawn_ctx = SpawnContext {
        parent_run_id: ctx.run_id.clone(),
        parent_agent_id: ctx.agent_id.clone(),
        recursion_depth: ctx.recursion_depth,
        working_dir: ctx.working_dir.clone(),
        inherited_permissions: Some(inherited_permissions),
        inherited_skills: ctx.active_skills.clone(),
    };

    // Execute spawn
    match ctx.spawner.spawn(input, &spawn_ctx).await {
        Ok(output) => serde_json::to_string(&output).unwrap_or_else(|_| {
            json!({
                "status": "failed",
                "error": "Failed to serialize output"
            })
            .to_string()
        }),
        Err(e) => json!({
            "status": "failed",
            "error": e.to_string()
        })
        .to_string(),
    }
}

// ─── get_agent_result tool ─────────────────────────────────────────────────

/// Handle get_agent_result tool call — retrieves a background child's result.
///
/// When a parent spawns a child with `background: true`, the child runs
/// asynchronously and the parent receives only a "launched" status. This
/// tool lets the parent poll for the child's result once it completes.
pub async fn handle_get_agent_result_tool(
    args: &Value,
    ctx: Option<&SpawnAgentContext>,
) -> String {
    let agent_id = match args.get("agent_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => {
            return json!({
                "status": "error",
                "error": "Missing required field: agent_id"
            })
            .to_string();
        }
    };

    let ctx = match ctx {
        Some(c) => c,
        None => {
            return json!({
                "status": "error",
                "error": "Agent spawning not available in this context."
            })
            .to_string();
        }
    };

    use astra_turn_core::orchestration_types::AgentStatus;

    // Check completed agents first.
    let completed = ctx.spawner.completed_agents_snapshot().await;
    if let Some(info) = completed.iter().find(|s| s.agent_id == agent_id) {
        match &info.status {
            AgentStatus::Completed { result } => {
                return json!({
                    "status": "completed",
                    "agent_id": agent_id,
                    "result": result,
                })
                .to_string();
            }
            AgentStatus::Failed { error } => {
                return json!({
                    "status": "failed",
                    "agent_id": agent_id,
                    "error": error,
                })
                .to_string();
            }
            _ => {}
        }
    }

    // Check active agents.
    let active = ctx.spawner.list_agents(&ctx.run_id).await;
    if let Some(info) = active.iter().find(|a| a.agent_id == agent_id) {
        return json!({
            "status": "running",
            "agent_id": agent_id,
            "description": info.description,
        })
        .to_string();
    }

    json!({
        "status": "not_found",
        "agent_id": agent_id,
        "error": format!("No agent with id '{agent_id}' found")
    })
    .to_string()
}

/// JSON schema for get_agent_result tool.
pub fn get_agent_result_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "get_agent_result",
            "description": "Retrieve the result of a background-spawned agent. Call this after spawn_agent with background:true to get the child's output once it finishes.",
            "parameters": {
                "type": "object",
                "properties": {
                    "agent_id": {
                        "type": "string",
                        "description": "The agent_id returned by spawn_agent."
                    }
                },
                "required": ["agent_id"]
            }
        }
    })
}

// ─── Tool Schema ───────────────────────────────────────────────────────────

/// Generate the JSON schema for spawn_agent tool.
///
/// Re-exports from astra_runtime::orchestration for convenience.
pub fn get_spawn_agent_schema() -> Value {
    spawn_agent_schema()
}
// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_structure() {
        let schema = get_spawn_agent_schema();
        assert_eq!(schema["type"], "function");
        assert_eq!(schema["function"]["name"], "spawn_agent");
        assert!(schema["function"]["parameters"]["properties"]["description"].is_object());
        assert!(schema["function"]["parameters"]["properties"]["prompt"].is_object());
    }

    #[test]
    fn get_agent_result_schema_has_agent_id() {
        let schema = get_agent_result_schema();
        assert_eq!(schema["type"], "function");
        assert_eq!(schema["function"]["name"], "get_agent_result");
        assert!(schema["function"]["parameters"]["properties"]["agent_id"].is_object());
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

    #[tokio::test]
    async fn get_agent_result_missing_agent_id() {
        let args = json!({});
        let result = handle_get_agent_result_tool(&args, None).await;
        assert!(result.contains("Missing required field"));
    }

    #[tokio::test]
    async fn get_agent_result_no_context() {
        let args = json!({"agent_id": "child-1"});
        let result = handle_get_agent_result_tool(&args, None).await;
        assert!(result.contains("not available"));
    }
}
