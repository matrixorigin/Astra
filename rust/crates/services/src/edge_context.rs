//! Typed edge context for cloud-side processing.
//!
//! Replaces loose `serde_json::Value` maps with structured types for the
//! context that edge (CLI) clients send to the cloud server. This gives
//! us compile-time guarantees and self-documenting fields instead of
//! stringly-typed JSON lookups.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Typed representation of the context an edge client sends to the cloud.
///
/// The CLI (thin-client) assembles this from the local environment and
/// sends it as the `context` field on `ChatRequestData`. The server
/// deserializes it into this struct for type-safe processing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EdgeContext {
    /// Tool definitions available on the edge (e.g., bash, write_file).
    #[serde(default)]
    pub edge_tools: Vec<serde_json::Value>,

    /// Edge environment profile (cwd, git branch, OS, etc.).
    #[serde(default)]
    pub edge_profile: EdgeProfile,

    /// Memoria / memory context pre-fetched by the edge.
    #[serde(default)]
    pub memoria_context: Option<serde_json::Value>,

    /// Skills available on the edge.
    #[serde(default)]
    pub edge_skills: Vec<EdgeSkillRef>,

    /// Additional untyped context (forward-compatible extensibility).
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl EdgeContext {
    /// Extract from a raw `serde_json::Map` (the `context` field of `ChatRequestData`).
    pub fn from_context_map(map: &serde_json::Map<String, serde_json::Value>) -> Self {
        serde_json::from_value(serde_json::Value::Object(map.clone())).unwrap_or_default()
    }

    /// Convert to the legacy `Map<String, Value>` for backward compatibility.
    pub fn to_context_map(&self) -> serde_json::Map<String, serde_json::Value> {
        match serde_json::to_value(self) {
            Ok(serde_json::Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        }
    }

    /// Whether the edge provided any tools.
    pub fn has_tools(&self) -> bool {
        !self.edge_tools.is_empty()
    }

    /// Number of tools available on the edge.
    pub fn tool_count(&self) -> usize {
        self.edge_tools.len()
    }

    /// Extract tool names from the edge tools definitions.
    pub fn tool_names(&self) -> Vec<String> {
        self.edge_tools
            .iter()
            .filter_map(|t| {
                t.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .map(String::from)
            })
            .collect()
    }
}

/// Structured edge environment profile.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EdgeProfile {
    /// Current working directory on the edge.
    #[serde(default)]
    pub cwd: Option<String>,

    /// Git branch name (if in a git repo).
    #[serde(default)]
    pub git_branch: Option<String>,

    /// Git repository root path.
    #[serde(default)]
    pub git_root: Option<String>,

    /// Operating system.
    #[serde(default)]
    pub os: Option<String>,

    /// Shell type (bash, zsh, etc.).
    #[serde(default)]
    pub shell: Option<String>,

    /// Agent version string.
    #[serde(default)]
    pub agent_version: Option<String>,

    /// Edge agent ID (for multi-agent coordination).
    #[serde(default)]
    pub edge_agent_id: Option<String>,

    /// Edge instance ID (for distinguishing multiple CLI instances).
    #[serde(default)]
    pub edge_id: Option<String>,

    /// Additional profile fields (forward-compatible).
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl EdgeProfile {
    /// Convert to a `Map<String, Value>` for backward compatibility.
    pub fn to_map(&self) -> serde_json::Map<String, serde_json::Value> {
        match serde_json::to_value(self) {
            Ok(serde_json::Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        }
    }
}

/// Reference to a skill available on the edge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeSkillRef {
    /// Skill name/identifier.
    pub name: String,
    /// Skill version.
    #[serde(default)]
    pub version: Option<String>,
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_context_from_empty_map() {
        let map = serde_json::Map::new();
        let ctx = EdgeContext::from_context_map(&map);
        assert!(!ctx.has_tools());
        assert_eq!(ctx.tool_count(), 0);
        assert!(ctx.tool_names().is_empty());
    }

    #[test]
    fn edge_context_from_full_map() {
        let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
            r#"{
                "edge_tools": [
                    {"function": {"name": "bash", "parameters": {}}},
                    {"function": {"name": "write_file", "parameters": {}}}
                ],
                "edge_profile": {
                    "cwd": "/home/user/project",
                    "git_branch": "main",
                    "os": "linux"
                },
                "edge_skills": [
                    {"name": "code-review", "version": "1.0"}
                ]
            }"#,
        )
        .unwrap();

        let ctx = EdgeContext::from_context_map(&map);
        assert!(ctx.has_tools());
        assert_eq!(ctx.tool_count(), 2);
        assert_eq!(ctx.tool_names(), vec!["bash", "write_file"]);
        assert_eq!(ctx.edge_profile.cwd.as_deref(), Some("/home/user/project"));
        assert_eq!(ctx.edge_profile.git_branch.as_deref(), Some("main"));
        assert_eq!(ctx.edge_profile.os.as_deref(), Some("linux"));
        assert_eq!(ctx.edge_skills.len(), 1);
        assert_eq!(ctx.edge_skills[0].name, "code-review");
    }

    #[test]
    fn edge_context_round_trip() {
        let ctx = EdgeContext {
            edge_tools: vec![serde_json::json!({"function": {"name": "bash"}})],
            edge_profile: EdgeProfile {
                cwd: Some("/tmp".into()),
                git_branch: Some("feature".into()),
                ..Default::default()
            },
            ..Default::default()
        };

        let map = ctx.to_context_map();
        let restored = EdgeContext::from_context_map(&map);
        assert_eq!(restored.tool_count(), 1);
        assert_eq!(restored.edge_profile.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn edge_context_preserves_unknown_fields() {
        let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
            r#"{
                "edge_tools": [],
                "custom_field": "hello",
                "another": 42
            }"#,
        )
        .unwrap();

        let ctx = EdgeContext::from_context_map(&map);
        assert_eq!(ctx.extra.get("custom_field").unwrap(), "hello");
        assert_eq!(ctx.extra.get("another").unwrap(), 42);
    }

    #[test]
    fn edge_profile_to_map_includes_all_fields() {
        let profile = EdgeProfile {
            cwd: Some("/workspace".into()),
            os: Some("darwin".into()),
            agent_version: Some("0.9.0".into()),
            ..Default::default()
        };

        let map = profile.to_map();
        assert_eq!(map.get("cwd").unwrap(), "/workspace");
        assert_eq!(map.get("os").unwrap(), "darwin");
        assert_eq!(map.get("agent_version").unwrap(), "0.9.0");
    }

    #[test]
    fn edge_profile_tolerates_extra_fields() {
        let json: serde_json::Value = serde_json::json!({
            "cwd": "/home",
            "git_branch": "dev",
            "custom_editor": "vim",
            "terminal_width": 120
        });
        let profile: EdgeProfile = serde_json::from_value(json).unwrap();
        assert_eq!(profile.cwd.as_deref(), Some("/home"));
        assert!(profile.extra.contains_key("custom_editor"));
        assert!(profile.extra.contains_key("terminal_width"));
    }

    #[test]
    fn tool_names_handles_missing_function_field() {
        let ctx = EdgeContext {
            edge_tools: vec![
                serde_json::json!({"function": {"name": "bash"}}),
                serde_json::json!({"type": "custom"}), // no function.name
                serde_json::json!({"function": {"name": "grep"}}),
            ],
            ..Default::default()
        };
        assert_eq!(ctx.tool_names(), vec!["bash", "grep"]);
    }

    #[test]
    fn edge_context_json_serialization() {
        let ctx = EdgeContext {
            edge_tools: vec![serde_json::json!({"function": {"name": "bash"}})],
            edge_profile: EdgeProfile {
                cwd: Some("/tmp".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let json = serde_json::to_string(&ctx).unwrap();
        assert!(json.contains("edge_tools"));
        assert!(json.contains("bash"));
        assert!(json.contains("/tmp"));
    }
}
