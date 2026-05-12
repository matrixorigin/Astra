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
pub async fn handle_get_agent_result_tool(args: &Value, ctx: Option<&SpawnAgentContext>) -> String {
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

    // Wait for the agent to complete using event notification — no polling.
    // The spawner's Notify is signaled by handle_completion, so this
    // returns as soon as the child finishes (or times out).
    let timeout = std::time::Duration::from_secs(120);
    match ctx.spawner.wait_for_agent(agent_id, timeout).await {
        Some(AgentStatus::Completed {
            result,
            finish_reason,
        }) => {
            // Surface `finish_reason` so the parent agent can
            // distinguish normal completion from budget-exhaustion
            // and other resumable interruptions without regex-
            // matching the output string. A value other than
            // `"normal"` signals "the child still had more work to
            // do" — the parent may want to spawn a continuation or
            // raise the budget.
            let reason = finish_reason.as_deref().unwrap_or("normal");
            json!({
                "status": "completed",
                "agent_id": agent_id,
                "result": result,
                "finish_reason": reason,
            })
            .to_string()
        }
        Some(AgentStatus::Failed {
            error,
            finish_reason,
        }) => {
            let reason = finish_reason.as_deref().unwrap_or("failed");
            json!({
                "status": "failed",
                "agent_id": agent_id,
                "error": error,
                "finish_reason": reason,
            })
            .to_string()
        }
        Some(status) => json!({
            "status": "unknown",
            "agent_id": agent_id,
            "detail": format!("{status:?}"),
        })
        .to_string(),
        None => {
            // P2 (session 8d9e5903 T10 regression): distinguish "truly
            // gone" from "still running — parent should call again".
            // Before this refinement, every wait timeout looked
            // identical to the LLM, so a still-executing child got
            // treated as dead. Now the parent sees "still_running"
            // when there is live state and "timeout" only when we
            // genuinely have nothing to report.
            let live_status = ctx
                .spawner
                .get_agent_state_any(agent_id)
                .await
                .map(|state| state.status);
            render_wait_timeout_outcome(agent_id, live_status.as_ref(), timeout)
        }
    }
}

/// Decide the outcome JSON when `wait_for_agent` returns `None`.
///
/// Split out as a pure function so the P2 decision (timeout vs
/// still_running) can be regression-tested without standing up a
/// full `DynamicAgentSpawner`.
pub(crate) fn render_wait_timeout_outcome(
    agent_id: &str,
    live_status: Option<&astra_turn_core::orchestration_types::AgentStatus>,
    timeout: std::time::Duration,
) -> String {
    match live_status {
        Some(status) if !status.is_terminal() => json!({
            "status": "still_running",
            "agent_id": agent_id,
            "current_status": format!("{status:?}"),
            "waited_secs": timeout.as_secs(),
            "hint": "The child agent is still working. Call `get_agent_result` again \
                    to continue waiting. Do NOT treat this as failure or fabricate \
                    what the child would have returned.",
        })
        .to_string(),
        _ => json!({
            "status": "timeout",
            "agent_id": agent_id,
            "error": format!(
                "Agent '{agent_id}' did not complete within {}s and has no live state",
                timeout.as_secs()
            ),
        })
        .to_string(),
    }
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

    // ── P2: wait-timeout decision (session 8d9e5903 T10 regression) ──

    use astra_turn_core::orchestration_types::AgentStatus;
    use std::time::Duration;

    #[test]
    fn wait_timeout_still_running_when_live_state_non_terminal() {
        let status = AgentStatus::Running {
            activity: "reading file".into(),
        };
        let out =
            render_wait_timeout_outcome("reviewer-tests", Some(&status), Duration::from_secs(120));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["status"], "still_running",
            "a timeout while the child is demonstrably running must NOT look like failure: {out}"
        );
        assert!(
            v["hint"].as_str().unwrap_or("").contains("still working"),
            "the hint must tell the LLM to call again rather than synthesize a fake result: {out}"
        );
        assert_eq!(v["waited_secs"], 120);
    }

    #[test]
    fn wait_timeout_timeout_when_no_live_state() {
        let out = render_wait_timeout_outcome("unknown-agent", None, Duration::from_secs(120));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["status"], "timeout",
            "when there is no live state the outcome is a genuine timeout: {out}"
        );
        assert!(
            v["error"].as_str().unwrap_or("").contains("no live state"),
            "error must make the distinction auditable: {out}"
        );
    }

    #[test]
    fn wait_timeout_reports_terminal_states_as_timeout_not_still_running() {
        // A terminal state (Failed / Cancelled / Completed) that
        // somehow slipped past the wait-for-agent completion path
        // should not be rendered as "still_running". Rare but
        // possible if completion_notifier was dropped on a race; we
        // still want the decision to be clear.
        for terminal in [
            AgentStatus::Cancelled,
            AgentStatus::Failed {
                error: "x".into(),
                finish_reason: None,
            },
            AgentStatus::Completed {
                result: "done".into(),
                finish_reason: None,
            },
        ] {
            let out = render_wait_timeout_outcome("ag", Some(&terminal), Duration::from_secs(30));
            let v: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(
                v["status"], "timeout",
                "terminal state must not render as still_running: status={terminal:?} out={out}"
            );
        }
    }
}
