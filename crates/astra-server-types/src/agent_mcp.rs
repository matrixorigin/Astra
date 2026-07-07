//! Per-Agent MCP Server Lifecycle (D-10)
//!
//! Manages MCP server connections scoped to individual agent executions.
//! When a SubRunExecutor starts a child agent whose `AgentProfile` has
//! `mcp_servers` set, this module connects the specified MCP servers
//! and provides their tools/skills to the agent. On agent completion,
//! connections are cleaned up.
//!
//! MCP server definitions come from project-level config
//! (e.g., `.astra/mcp.json` or `.astra/mcp.yaml`).

use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

// ───────────────────────────── MCP Server Config ────────────────────────

/// Definition of an MCP server from project config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Unique name (referenced by AgentProfile.mcp_servers).
    pub name: String,
    /// Command to start the server (e.g., "npx @modelcontextprotocol/server-github").
    pub command: String,
    /// Command arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables for the server process.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Working directory (defaults to project root).
    pub cwd: Option<String>,
    /// Connection timeout in seconds.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    30
}

/// Project-level MCP configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpProjectConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

impl McpProjectConfig {
    /// Load MCP config from `.astra/mcp.json` or `.astra/mcp.yaml`.
    pub fn load_from_project(project_root: &Path) -> Self {
        let json_path = project_root.join(".astra/mcp.json");
        let yaml_path = project_root.join(".astra/mcp.yaml");

        if json_path.exists()
            && let Ok(content) = std::fs::read_to_string(&json_path)
            && let Ok(config) = serde_json::from_str(&content)
        {
            return config;
        }

        if yaml_path.exists()
            && let Ok(content) = std::fs::read_to_string(&yaml_path)
            && let Ok(config) = serde_yaml_ng::from_str(&content)
        {
            return config;
        }

        Self::default()
    }

    /// Look up a server by name.
    pub fn get_server(&self, name: &str) -> Option<&McpServerConfig> {
        self.servers.iter().find(|s| s.name == name)
    }
}

// ───────────────────────────── Agent MCP Session ────────────────────────

/// Represents a set of MCP connections active for one agent execution.
#[derive(Debug)]
pub struct AgentMcpSession {
    /// Agent ID this session belongs to.
    pub agent_id: String,
    /// Connected server names.
    pub connected_servers: Vec<String>,
    /// Tools discovered from connected MCP servers.
    pub discovered_tools: Vec<McpDiscoveredTool>,
    /// When the session was created.
    pub created_at: SystemTime,
}

/// A tool discovered from an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpDiscoveredTool {
    /// MCP server this tool comes from.
    pub server_name: String,
    /// Tool name as reported by MCP.
    pub tool_name: String,
    /// Tool description.
    pub description: String,
    /// JSON Schema for the tool's input.
    pub input_schema: serde_json::Value,
}

impl AgentMcpSession {
    /// Create a new session for an agent.
    pub fn new(agent_id: &str) -> Self {
        Self {
            agent_id: agent_id.to_string(),
            connected_servers: Vec::new(),
            discovered_tools: Vec::new(),
            created_at: SystemTime::now(),
        }
    }

    /// Record that an MCP server was connected.
    pub fn add_server(&mut self, server_name: &str) {
        if !self.connected_servers.contains(&server_name.to_string()) {
            self.connected_servers.push(server_name.to_string());
        }
    }

    /// Add tools discovered from an MCP server.
    pub fn add_tools(&mut self, tools: Vec<McpDiscoveredTool>) {
        self.discovered_tools.extend(tools);
    }

    /// Get all tool names available in this session.
    pub fn tool_names(&self) -> Vec<String> {
        self.discovered_tools
            .iter()
            .map(|t| t.tool_name.clone())
            .collect()
    }
}

// ───────────────────────────── Lifecycle Manager ────────────────────────

/// Manages MCP server lifecycles scoped to agent executions.
pub struct McpLifecycleManager {
    /// Project-level MCP configuration.
    config: McpProjectConfig,
    /// Active sessions by agent_id.
    sessions: std::sync::Mutex<HashMap<String, AgentMcpSession>>,
}

impl McpLifecycleManager {
    pub fn new(config: McpProjectConfig) -> Self {
        Self {
            config,
            sessions: std::sync::Mutex::new(HashMap::new()),
        }
    }
    /// Start MCP connections for an agent.
    /// Returns the list of tool names discovered.
    pub fn start_agent_session(
        &self,
        agent_id: &str,
        mcp_server_names: &[String],
    ) -> Result<Vec<String>, String> {
        let mut session = AgentMcpSession::new(agent_id);
        let mut errors = Vec::new();

        for name in mcp_server_names {
            match self.config.get_server(name) {
                Some(server_config) => {
                    // In a real implementation, this would:
                    // 1. Spawn the MCP server process
                    // 2. Establish stdio/SSE transport
                    // 3. Call initialize/tools.list
                    // 4. Collect tool schemas
                    //
                    // For now, record the connection intent.
                    session.add_server(name);
                    eprintln!(
                        "[mcp] Agent '{}': connected to MCP server '{}' (cmd: {} {})",
                        agent_id,
                        name,
                        server_config.command,
                        server_config.args.join(" ")
                    );
                }
                None => {
                    errors.push(format!("MCP server '{}' not found in project config", name));
                }
            }
        }

        let tool_names = session.tool_names();
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(agent_id.to_string(), session);

        if !errors.is_empty() {
            eprintln!(
                "[mcp] Agent '{}': warnings: {}",
                agent_id,
                errors.join("; ")
            );
        }

        Ok(tool_names)
    }

    /// Stop MCP connections for an agent (cleanup on agent completion).
    pub fn stop_agent_session(&self, agent_id: &str) -> Option<AgentMcpSession> {
        let session = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(agent_id);
        if let Some(ref s) = session {
            for name in &s.connected_servers {
                eprintln!(
                    "[mcp] Agent '{}': disconnected from MCP server '{}'",
                    agent_id, name
                );
            }
        }
        session
    }

    /// Check if an agent has an active MCP session.
    pub fn has_session(&self, agent_id: &str) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(agent_id)
    }

    /// Get discovered tools for an agent.
    pub fn agent_tools(&self, agent_id: &str) -> Vec<McpDiscoveredTool> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(agent_id)
            .map(|s| s.discovered_tools.clone())
            .unwrap_or_default()
    }

    /// List all active agent sessions.
    pub fn active_sessions(&self) -> Vec<String> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }
}

// ───────────────────────────── Tests ─────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> McpProjectConfig {
        McpProjectConfig {
            servers: vec![
                McpServerConfig {
                    name: "github".into(),
                    command: "npx".into(),
                    args: vec!["@modelcontextprotocol/server-github".into()],
                    env: HashMap::from([("GITHUB_TOKEN".into(), "xxx".into())]),
                    cwd: None,
                    timeout_secs: 30,
                },
                McpServerConfig {
                    name: "filesystem".into(),
                    command: "npx".into(),
                    args: vec![
                        "@modelcontextprotocol/server-filesystem".into(),
                        "/tmp".into(),
                    ],
                    env: HashMap::new(),
                    cwd: None,
                    timeout_secs: 15,
                },
            ],
        }
    }

    #[test]
    fn config_lookup() {
        let config = sample_config();
        assert!(config.get_server("github").is_some());
        assert!(config.get_server("filesystem").is_some());
        assert!(config.get_server("nonexistent").is_none());
    }

    #[test]
    fn lifecycle_start_stop() {
        let mgr = McpLifecycleManager::new(sample_config());

        let _tools = mgr
            .start_agent_session("coder-1", &["github".into()])
            .unwrap();
        assert!(mgr.has_session("coder-1"));

        let session = mgr.stop_agent_session("coder-1");
        assert!(session.is_some());
        assert!(!mgr.has_session("coder-1"));
    }

    #[test]
    fn lifecycle_missing_server() {
        let mgr = McpLifecycleManager::new(sample_config());

        let result = mgr.start_agent_session("agent-1", &["nonexistent".into()]);
        assert!(result.is_ok()); // warns but doesn't fail
    }

    #[test]
    fn multiple_servers() {
        let mgr = McpLifecycleManager::new(sample_config());

        mgr.start_agent_session("agent-1", &["github".into(), "filesystem".into()])
            .unwrap();

        assert!(mgr.has_session("agent-1"));
        let active = mgr.active_sessions();
        assert_eq!(active.len(), 1);
    }

    #[test]
    fn session_tools() {
        let mgr = McpLifecycleManager::new(sample_config());
        mgr.start_agent_session("agent-1", &["github".into()])
            .unwrap();

        // No tools discovered yet (real impl would call tools.list)
        let tools = mgr.agent_tools("agent-1");
        assert!(tools.is_empty());
    }

    #[test]
    fn stop_nonexistent() {
        let mgr = McpLifecycleManager::new(sample_config());
        assert!(mgr.stop_agent_session("ghost").is_none());
    }

    #[test]
    fn load_from_empty_dir() {
        let config = McpProjectConfig::load_from_project(Path::new("/nonexistent"));
        assert!(config.servers.is_empty());
    }

    #[test]
    fn agent_mcp_session_dedup() {
        let mut session = AgentMcpSession::new("test");
        session.add_server("github");
        session.add_server("github"); // duplicate
        assert_eq!(session.connected_servers.len(), 1);
    }

    #[test]
    fn config_json_parse() {
        let json = r#"{
            "servers": [{
                "name": "test",
                "command": "echo",
                "args": ["hello"],
                "timeout_secs": 10
            }]
        }"#;
        let config: McpProjectConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].name, "test");
        assert_eq!(config.servers[0].timeout_secs, 10);
    }
}
