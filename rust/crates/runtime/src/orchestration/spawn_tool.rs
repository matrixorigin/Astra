//! Spawn agent tool schema and types.

use serde::{Deserialize, Serialize};
use serde_json::json;

/// Input for the spawn_agent tool.
#[derive(Debug, Clone, Deserialize)]
pub struct SpawnAgentInput {
    /// Short (3-5 word) description of the task.
    pub description: String,

    /// Detailed task prompt for the agent.
    pub prompt: String,

    /// Agent type: "explore", "code-review", "task", "general-purpose".
    #[serde(default = "default_agent_type")]
    pub agent_type: String,

    /// Optional model override.
    pub model: Option<String>,

    /// Run in background (async) - default true.
    #[serde(default = "default_true")]
    pub background: bool,

    /// Name for agent-to-agent messaging.
    pub name: Option<String>,

    /// Max turns before auto-stopping.
    pub max_turns: Option<u32>,

    /// Create isolated git worktree for this agent.
    #[serde(default)]
    pub isolated: bool,

    /// Tool allowlist (overrides agent_type defaults).
    pub allowed_tools: Option<Vec<String>>,
}

fn default_agent_type() -> String {
    "general-purpose".to_string()
}

fn default_true() -> bool {
    true
}

/// Output from spawn_agent tool.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SpawnAgentOutput {
    /// Agent completed synchronously.
    Completed {
        agent_id: String,
        result: String,
        tool_calls: u32,
        duration_ms: u64,
    },
    /// Agent was cancelled synchronously.
    Cancelled {
        agent_id: String,
        reason: String,
        tool_calls: u32,
        duration_ms: u64,
    },
    /// Agent is waiting for external input synchronously.
    Waiting {
        agent_id: String,
        reason: String,
        tool_calls: u32,
        duration_ms: u64,
    },
    /// Agent launched in background.
    Launched {
        agent_id: String,
        description: String,
        messaging_address: Option<String>,
    },
    /// Failed to spawn.
    Failed { error: String },
}

impl SpawnAgentOutput {
    pub fn launched(agent_id: impl Into<String>, description: impl Into<String>) -> Self {
        Self::Launched {
            agent_id: agent_id.into(),
            description: description.into(),
            messaging_address: None,
        }
    }

    pub fn with_address(self, address: Option<String>) -> Self {
        match self {
            Self::Launched {
                agent_id,
                description,
                ..
            } => Self::Launched {
                agent_id,
                description,
                messaging_address: address,
            },
            other => other,
        }
    }
}

/// Generate the JSON schema for spawn_agent tool.
/// Returns a schema in the standard format: `{ type: "function", function: { name, description, parameters } }`.
pub fn spawn_agent_schema() -> serde_json::Value {
    json!({
        "type": "function",
        "function": {
            "name": "spawn_agent",
            "description": "Launch a specialized sub-agent to perform a task. Agents run autonomously and return results. Use for parallel work, independent research, code review, or any task that benefits from dedicated focus. Agent types: 'explore' (fast codebase research), 'code-review' (analyze changes), 'task' (run commands), 'general-purpose' (full capabilities).",
            "parameters": {
                "type": "object",
                "properties": {
                    "description": {
                        "type": "string",
                        "description": "A short (3-5 word) description of the task."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Detailed task prompt for the agent. Be specific about what you want."
                    },
                    "agent_type": {
                        "type": "string",
                        "enum": ["explore", "code-review", "task", "general-purpose"],
                        "description": "Type of specialized agent. 'explore' for research, 'code-review' for reviewing changes, 'task' for running commands, 'general-purpose' for complex multi-step tasks.",
                        "default": "general-purpose"
                    },
                    "model": {
                        "type": "string",
                        "description": "Optional model override (e.g., 'claude-sonnet', 'claude-opus', 'claude-haiku')."
                    },
                    "background": {
                        "type": "boolean",
                        "description": "Run in background (async). If true, returns immediately with agent_id. Default: true.",
                        "default": true
                    },
                    "name": {
                        "type": "string",
                        "description": "Name for agent-to-agent messaging. Makes agent addressable via send_message."
                    },
                    "max_turns": {
                        "type": "integer",
                        "description": "Max turns before stopping. Default varies by agent_type.",
                        "minimum": 1,
                        "maximum": 100
                    },
                    "isolated": {
                        "type": "boolean",
                        "description": "Create isolated git worktree for this agent.",
                        "default": false
                    },
                    "allowed_tools": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Tool allowlist (overrides agent_type defaults)."
                    }
                },
                "required": ["description", "prompt"]
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spawn_agent_schema() {
        let schema = spawn_agent_schema();
        assert_eq!(schema["type"], "function");
        assert_eq!(schema["function"]["name"], "spawn_agent");
        assert!(schema["function"]["parameters"]["properties"]["description"].is_object());
    }

    #[test]
    fn test_deserialize_input() {
        let json = r#"{"description": "Test", "prompt": "Do the thing"}"#;
        let input: SpawnAgentInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.description, "Test");
        assert_eq!(input.agent_type, "general-purpose");
        assert!(input.background);
    }
}
