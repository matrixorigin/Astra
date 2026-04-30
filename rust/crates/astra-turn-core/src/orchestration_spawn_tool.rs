//! Spawn agent tool schema and types.

use serde::{Deserialize, Serialize};
use serde_json::json;

/// Request to inherit the parent's cacheable prefix when spawning.
///
/// When present in a `SpawnAgentInput`, the runtime looks up the
/// captured `ForkPrefix` for `from_run_id` (defaulting to the caller's
/// own run) and validates compatibility (provider, model, thinking
/// budget, size). If lookup or validation fails:
/// - `required: false` (default) — spawn proceeds without cache
///   inheritance and a telemetry event is emitted.
/// - `required: true` — spawn fails with an error.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct InheritPrefixSpec {
    /// Parent run id to inherit from. `None` means "the run calling
    /// spawn_agent" — the resolver substitutes the caller's run id
    /// at resolution time.
    #[serde(default)]
    pub from_run_id: Option<String>,

    /// Whether a missing or incompatible prefix is a hard failure.
    /// Default `false` keeps the spawn robust to eviction / TTL /
    /// feature-flag transitions.
    #[serde(default)]
    pub required: bool,
}

/// Input for the spawn_agent tool.
///
/// **Field order is load-bearing.** The struct is serialized to
/// JSON for the spawn_agent tool schema and included in tool-schema
/// cache-break attribution (see `cache_diagnostics.rs::per_tool_hashes`).
/// Reordering fields changes the canonical JSON bytes and invalidates
/// every captured parent prefix across a deploy. Add new fields at
/// the end; never reorder existing ones without a coordinated
/// migration.
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

    /// Max output tokens for the child's first API call. When a
    /// `ForkPrefix` is inherited with thinking enabled, the
    /// resolver will refuse the inheritance if this cap would
    /// clamp the effective thinking budget below the captured
    /// parent value (cache key drift).
    #[serde(default)]
    pub max_output_tokens: Option<u32>,

    /// Create isolated git worktree for this agent.
    #[serde(default)]
    pub isolated: bool,

    /// Tool allowlist (overrides agent_type defaults).
    pub allowed_tools: Option<Vec<String>>,

    /// Optional request to reuse a captured parent ForkPrefix for
    /// cache inheritance. When absent, spawn proceeds with a fresh
    /// prefix (no cache reuse). See [`InheritPrefixSpec`].
    #[serde(default)]
    pub inherit_prefix: Option<InheritPrefixSpec>,
}

impl Default for SpawnAgentInput {
    /// Mirror the serde `#[serde(default ...)]` defaults so struct
    /// literals using `..Default::default()` produce the same
    /// instance as an empty JSON `{"description": "", "prompt": ""}`.
    fn default() -> Self {
        Self {
            description: String::new(),
            prompt: String::new(),
            agent_type: default_agent_type(),
            model: None,
            background: default_true(),
            name: None,
            max_turns: None,
            max_output_tokens: None,
            isolated: false,
            allowed_tools: None,
            inherit_prefix: None,
        }
    }
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
                    },
                    "max_output_tokens": {
                        "type": "integer",
                        "description": "Max output tokens for the child's first API call. Interacts with prefix inheritance — see inherit_prefix.",
                        "minimum": 1
                    },
                    "inherit_prefix": {
                        "type": "object",
                        "description": "Inherit the parent's cacheable prefix so the child's first API request reuses the parent's prompt cache. Requires the ASTRA_FORK_INHERIT_PREFIX feature flag and a matching captured parent prefix.",
                        "properties": {
                            "from_run_id": {
                                "type": "string",
                                "description": "Parent run id to inherit from. Omit to inherit from the caller's own run."
                            },
                            "required": {
                                "type": "boolean",
                                "description": "If true, spawn fails when the prefix is missing or incompatible. Default false proceeds without cache reuse.",
                                "default": false
                            }
                        }
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
        // Inheritance defaults to None — existing clients get no
        // behavior change when they don't set inherit_prefix.
        assert!(input.inherit_prefix.is_none());
        assert!(input.max_output_tokens.is_none());
    }

    #[test]
    fn test_deserialize_inherit_prefix_defaults() {
        let json = r#"{
            "description": "D",
            "prompt": "P",
            "inherit_prefix": {}
        }"#;
        let input: SpawnAgentInput = serde_json::from_str(json).unwrap();
        let spec = input.inherit_prefix.expect("inherit_prefix present");
        assert_eq!(spec.from_run_id, None);
        assert!(!spec.required, "required defaults to false");
    }

    #[test]
    fn test_deserialize_inherit_prefix_explicit() {
        let json = r#"{
            "description": "D",
            "prompt": "P",
            "inherit_prefix": {"from_run_id": "run-parent", "required": true},
            "max_output_tokens": 8000
        }"#;
        let input: SpawnAgentInput = serde_json::from_str(json).unwrap();
        let spec = input.inherit_prefix.unwrap();
        assert_eq!(spec.from_run_id.as_deref(), Some("run-parent"));
        assert!(spec.required);
        assert_eq!(input.max_output_tokens, Some(8000));
    }

    #[test]
    fn test_schema_exposes_inherit_prefix() {
        let schema = spawn_agent_schema();
        let props = &schema["function"]["parameters"]["properties"];
        assert!(props["inherit_prefix"].is_object());
        assert!(props["max_output_tokens"].is_object());
    }
}
