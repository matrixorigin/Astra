//! Team configuration — YAML-based custom agent type definitions.
//!
//! Users can define custom agent types in `~/.astra/teams/` or `.astra/teams/`
//! using YAML files that extend or override built-in agent types.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::orchestration_builtin_agents::{AgentTypeDefinition, get_builtin_agent_types};

// ─── YAML Schema ────────────────────────────────────────────────────────────

/// YAML schema for a custom agent type definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTypeConfig {
    /// Agent type name (must be unique across builtins + custom).
    pub name: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// Optional base type to inherit from (e.g., "explore", "task").
    #[serde(default)]
    pub extends: Option<String>,
    /// System prompt addendum (appended to base if extending).
    #[serde(default)]
    pub system_prompt: String,
    /// Default model override.
    #[serde(default)]
    pub model: Option<String>,
    /// Max turns override.
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// Allowed tools (overrides base if set, or uses base tools).
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// Read-only flag override.
    #[serde(default)]
    pub read_only: Option<bool>,
}

/// YAML team definition file containing one or more agent types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamConfigFile {
    /// List of custom agent type definitions.
    pub agents: Vec<AgentTypeConfig>,
}

// ─── Agent Registry ─────────────────────────────────────────────────────────

/// Unified registry of agent types: builtins + user-defined.
///
/// Resolves agent type names to definitions, with user-defined types
/// taking precedence over builtins with the same name.
#[derive(Debug, Clone)]
pub struct AgentRegistry {
    /// All agent definitions, indexed by type name.
    agents: Vec<AgentTypeDefinition>,
    /// User-defined type names (for display/listing purposes).
    custom_names: HashSet<String>,
}

impl AgentRegistry {
    /// Create a registry with only built-in agent types.
    pub fn builtins_only() -> Self {
        Self {
            agents: get_builtin_agent_types(),
            custom_names: HashSet::new(),
        }
    }

    /// Create a registry by discovering configs from standard paths.
    ///
    /// Search order: `.astra/teams/` (project), `~/.astra/teams/` (user).
    /// Project-level configs override user-level for same-named types.
    pub fn discover(project_root: Option<&Path>) -> Self {
        let mut registry = Self::builtins_only();

        // User-level configs (lower priority)
        if let Some(home) = dirs::home_dir() {
            let user_dir = home.join(".astra").join("teams");
            registry.load_from_dir(&user_dir);
        }

        // Project-level configs (higher priority — overrides user)
        if let Some(root) = project_root {
            let project_dir = root.join(".astra").join("teams");
            registry.load_from_dir(&project_dir);
        }

        registry
    }

    /// Load all YAML configs from a directory.
    fn load_from_dir(&mut self, dir: &Path) {
        if !dir.is_dir() {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|ext| ext == "yaml" || ext == "yml")
            {
                if let Err(e) = self.load_file(&path) {
                    eprintln!("  ⚠ team config: failed to load {}: {}", path.display(), e);
                }
            }
        }
    }

    /// Load and merge a single YAML config file.
    fn load_file(&mut self, path: &Path) -> Result<(), String> {
        let content = std::fs::read_to_string(path).map_err(|e| format!("read error: {e}"))?;
        let config: TeamConfigFile =
            serde_yaml_ng::from_str(&content).map_err(|e| format!("YAML parse error: {e}"))?;

        for agent_config in config.agents {
            let def = self.resolve_config(agent_config)?;
            // Remove existing with same name (override)
            self.agents.retain(|a| a.agent_type != def.agent_type);
            self.custom_names.insert(def.agent_type.clone());
            self.agents.push(def);
        }
        Ok(())
    }

    /// Resolve an agent config into a full definition, inheriting from base if specified.
    fn resolve_config(&self, config: AgentTypeConfig) -> Result<AgentTypeDefinition, String> {
        if let Some(ref base_name) = config.extends {
            let base = self
                .get(base_name)
                .ok_or_else(|| format!("base type '{}' not found", base_name))?;

            Ok(AgentTypeDefinition {
                agent_type: config.name,
                description: if config.description.is_empty() {
                    base.description.clone()
                } else {
                    config.description
                },
                system_prompt_addendum: if config.system_prompt.is_empty() {
                    base.system_prompt_addendum.clone()
                } else {
                    format!("{}\n{}", base.system_prompt_addendum, config.system_prompt)
                },
                default_model: config.model.unwrap_or_else(|| base.default_model.clone()),
                max_turns: config.max_turns.unwrap_or(base.max_turns),
                allowed_tools: config
                    .allowed_tools
                    .map(|t| t.into_iter().collect())
                    .unwrap_or_else(|| base.allowed_tools.clone()),
                read_only: config.read_only.unwrap_or(base.read_only),
            })
        } else {
            Ok(AgentTypeDefinition {
                agent_type: config.name,
                description: config.description,
                system_prompt_addendum: config.system_prompt,
                default_model: config.model.unwrap_or_else(|| "claude-sonnet".to_string()),
                max_turns: config.max_turns.unwrap_or(30),
                allowed_tools: config
                    .allowed_tools
                    .map(|t| t.into_iter().collect())
                    .unwrap_or_else(|| ["*"].into_iter().map(String::from).collect()),
                read_only: config.read_only.unwrap_or(false),
            })
        }
    }

    /// Get a definition by type name.
    pub fn get(&self, agent_type: &str) -> Option<AgentTypeDefinition> {
        self.agents
            .iter()
            .find(|a| a.agent_type == agent_type)
            .cloned()
    }

    /// List all available agent type definitions.
    pub fn list_all(&self) -> &[AgentTypeDefinition] {
        &self.agents
    }

    /// Check if a type name is user-defined (vs built-in).
    pub fn is_custom(&self, agent_type: &str) -> bool {
        self.custom_names.contains(agent_type)
    }
    /// Register a custom agent type at runtime (non-persistent).
    pub fn register(&mut self, def: AgentTypeDefinition) {
        self.agents.retain(|a| a.agent_type != def.agent_type);
        self.custom_names.insert(def.agent_type.clone());
        self.agents.push(def);
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::builtins_only()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_builtins() {
        let reg = AgentRegistry::builtins_only();
        assert_eq!(reg.list_all().len(), 4);
        assert!(reg.get("explore").is_some());
        assert!(reg.get("task").is_some());
        assert!(reg.get("unknown").is_none());
    }

    #[test]
    fn test_resolve_standalone_config() {
        let reg = AgentRegistry::builtins_only();
        let config = AgentTypeConfig {
            name: "security-scanner".to_string(),
            description: "Scan for vulnerabilities".to_string(),
            extends: None,
            system_prompt: "You are a security expert.".to_string(),
            model: Some("claude-sonnet".to_string()),
            max_turns: Some(10),
            allowed_tools: Some(vec!["grep".to_string(), "view".to_string()]),
            read_only: Some(true),
        };
        let def = reg.resolve_config(config).unwrap();
        assert_eq!(def.agent_type, "security-scanner");
        assert_eq!(def.max_turns, 10);
        assert!(def.read_only);
        assert!(def.allowed_tools.contains("grep"));
    }

    #[test]
    fn test_resolve_inherited_config() {
        let reg = AgentRegistry::builtins_only();
        let config = AgentTypeConfig {
            name: "deep-explore".to_string(),
            description: String::new(),
            extends: Some("explore".to_string()),
            system_prompt: "Focus on architecture.".to_string(),
            model: None,
            max_turns: Some(40),
            allowed_tools: None,
            read_only: None,
        };
        let def = reg.resolve_config(config).unwrap();
        assert_eq!(def.agent_type, "deep-explore");
        assert_eq!(def.max_turns, 40);
        assert!(def.read_only); // inherited from explore
        assert_eq!(def.default_model, "claude-haiku"); // inherited
        assert!(def.system_prompt_addendum.contains("architecture"));
    }

    #[test]
    fn test_register_overrides_existing() {
        let mut reg = AgentRegistry::builtins_only();
        assert_eq!(reg.get("explore").unwrap().max_turns, 20);

        reg.register(AgentTypeDefinition {
            agent_type: "explore".to_string(),
            description: "Custom explore".to_string(),
            system_prompt_addendum: String::new(),
            default_model: "claude-opus".to_string(),
            max_turns: 100,
            allowed_tools: ["*"].into_iter().map(String::from).collect(),
            read_only: false,
        });

        assert_eq!(reg.get("explore").unwrap().max_turns, 100);
        assert!(reg.is_custom("explore"));
    }

    #[test]
    fn test_yaml_parse() {
        let yaml = r#"
agents:
  - name: test-agent
    description: A test agent
    extends: task
    model: claude-haiku
    max_turns: 5
    allowed_tools:
      - bash
      - view
    read_only: true
"#;
        let config: TeamConfigFile =
            serde_yaml_ng::from_str(yaml).expect("fixture YAML must parse");
        assert_eq!(config.agents.len(), 1);
        assert_eq!(config.agents[0].name, "test-agent");
        assert_eq!(config.agents[0].extends, Some("task".to_string()));
    }
}
