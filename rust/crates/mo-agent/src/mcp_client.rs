//! MCP (Model Context Protocol) client for external tool integration.
//!
//! This module provides a client implementation for connecting to MCP servers,
//! allowing mo-agent to leverage external tools and resources exposed via MCP.
//!
//! # Usage
//!
//! ```rust,ignore
//! use mcp_client::{McpClient, McpServerConfig, Transport};
//!
//! // Configure a stdio-based MCP server
//! let config = McpServerConfig {
//!     name: "filesystem".to_string(),
//!     transport: Transport::Stdio {
//!         command: vec!["npx".to_string(), "@modelcontextprotocol/server-filesystem".to_string()],
//!         args: vec!["/workspace".to_string()],
//!         env: Default::default(),
//!     },
//! };
//!
//! // Connect and list tools
//! let client = McpClient::connect(config).await?;
//! let tools = client.list_tools().await?;
//! ```

// Connection helpers are staged for upcoming MCP wiring; keep the surface without warning spam.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use rmcp::{
    model::{CallToolRequestParams, CallToolResult, Tool},
    service::ServiceError,
    serve_client, ClientHandler, Peer, RoleClient,
    transport::TokioChildProcess,
};
use tokio::sync::RwLock;

/// MCP server transport configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Transport {
    /// Stdio transport: communicates via stdin/stdout with a child process.
    Stdio {
        /// Command to spawn (e.g., "npx", "python").
        command: Vec<String>,
        /// Additional arguments for the command.
        #[serde(default)]
        args: Vec<String>,
        /// Environment variables for the child process.
        #[serde(default)]
        env: HashMap<String, String>,
    },
}

/// Configuration for an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Unique name for this server.
    pub name: String,
    /// Transport configuration.
    pub transport: Transport,
    /// Optional description.
    #[serde(default)]
    pub description: String,
    /// Whether the server is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// MCP client handler that does nothing (default implementation).
#[derive(Debug, Default, Clone)]
struct NoOpClientHandler;

impl ClientHandler for NoOpClientHandler {}

/// Running MCP client connection.
pub struct McpConnection {
    /// Server name.
    pub name: String,
    /// Peer for sending requests.
    peer: Peer<RoleClient>,
    /// Cached tools from this server.
    tools: Vec<Tool>,
}

impl McpConnection {
    /// Get the server name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get available tools from this server.
    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    /// Refresh the tool list from the server.
    pub async fn refresh_tools(&mut self) -> Result<&[Tool], ServiceError> {
        self.tools = self.peer.list_all_tools().await?;
        Ok(&self.tools)
    }

    /// Call a tool on this server.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult, ServiceError> {
        let arguments = match arguments {
            serde_json::Value::Object(map) => Some(map),
            serde_json::Value::Null => None,
            _ => Some(serde_json::Map::from_iter([(
                "input".to_string(),
                arguments,
            )])),
        };
        let params = if let Some(args) = arguments {
            CallToolRequestParams::new(name.to_string()).with_arguments(args)
        } else {
            CallToolRequestParams::new(name.to_string())
        };
        self.peer.call_tool(params).await
    }

    /// Check if this server has a specific tool.
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.name == name)
    }
}

/// MCP client manager for multiple server connections.
#[derive(Default)]
pub struct McpClientManager {
    /// Active connections indexed by server name.
    connections: HashMap<String, Arc<McpConnection>>,
}

impl McpClientManager {
    /// Create a new empty client manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Connect to an MCP server.
    pub async fn connect(&mut self, config: McpServerConfig) -> Result<(), McpError> {
        if !config.enabled {
            return Ok(());
        }

        let name = config.name.clone();
        let connection = connect_to_server(config).await?;
        self.connections.insert(name, Arc::new(connection));
        Ok(())
    }

    /// Disconnect from an MCP server.
    pub fn disconnect(&mut self, name: &str) -> bool {
        self.connections.remove(name).is_some()
    }

    /// Get a connection by name.
    pub fn get(&self, name: &str) -> Option<Arc<McpConnection>> {
        self.connections.get(name).cloned()
    }

    /// List all connected server names.
    pub fn connected_servers(&self) -> Vec<&str> {
        self.connections.keys().map(|s| s.as_str()).collect()
    }

    /// Get all tools from all connected servers.
    pub fn all_tools(&self) -> Vec<(&str, &Tool)> {
        self.connections
            .iter()
            .flat_map(|(name, conn)| conn.tools.iter().map(move |t| (name.as_str(), t)))
            .collect()
    }

    /// Find which server has a specific tool.
    pub fn find_tool(&self, tool_name: &str) -> Option<(&str, &Tool)> {
        for (server_name, conn) in &self.connections {
            for tool in &conn.tools {
                if tool.name == tool_name {
                    return Some((server_name, tool));
                }
            }
        }
        None
    }

    /// Call a tool, automatically routing to the correct server.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult, McpError> {
        let (server_name, _) = self
            .find_tool(tool_name)
            .ok_or_else(|| McpError::ToolNotFound(tool_name.to_string()))?;

        let conn = self
            .connections
            .get(server_name)
            .ok_or_else(|| McpError::ServerNotConnected(server_name.to_string()))?;

        conn.call_tool(tool_name, arguments)
            .await
            .map_err(McpError::Service)
    }

    /// Number of active connections.
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }
}

/// Thread-safe MCP client manager.
pub type SharedMcpClientManager = Arc<RwLock<McpClientManager>>;

/// Create a new shared MCP client manager.
pub fn new_shared_manager() -> SharedMcpClientManager {
    Arc::new(RwLock::new(McpClientManager::new()))
}

/// Connect to an MCP server.
async fn connect_to_server(config: McpServerConfig) -> Result<McpConnection, McpError> {
    match config.transport {
        Transport::Stdio { command, args, env } => {
            connect_stdio(&config.name, &command, &args, &env).await
        }
    }
}

/// Connect via stdio transport.
async fn connect_stdio(
    name: &str,
    command: &[String],
    args: &[String],
    env: &HashMap<String, String>,
) -> Result<McpConnection, McpError> {
    if command.is_empty() {
        return Err(McpError::InvalidConfig("command cannot be empty".to_string()));
    }

    // Build the command using tokio::process::Command
    let mut cmd = tokio::process::Command::new(&command[0]);
    if command.len() > 1 {
        cmd.args(&command[1..]);
    }
    cmd.args(args);
    for (key, value) in env {
        cmd.env(key, value);
    }

    // Create child process transport
    let transport = TokioChildProcess::new(cmd)
        .map_err(|e| McpError::Spawn(e.to_string()))?;

    // Connect as MCP client
    let running = serve_client(NoOpClientHandler, transport)
        .await
        .map_err(|e| McpError::Initialize(e.to_string()))?;

    let peer = running.peer().clone();

    // Fetch initial tool list
    let tools = peer.list_all_tools().await.map_err(McpError::Service)?;

    Ok(McpConnection {
        name: name.to_string(),
        peer,
        tools,
    })
}

/// MCP client errors.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("Failed to spawn MCP server: {0}")]
    Spawn(String),

    #[error("Failed to initialize MCP connection: {0}")]
    Initialize(String),

    #[error("MCP service error: {0}")]
    Service(#[from] ServiceError),

    #[error("Tool not found: {0}")]
    ToolNotFound(String),

    #[error("Server not connected: {0}")]
    ServerNotConnected(String),
}

/// Convert MCP Tool to mo-agent tool schema format.
pub fn mcp_tool_to_schema(server_name: &str, tool: &Tool) -> serde_json::Value {
    // input_schema is Arc<JsonObject>, convert to Value
    let params = serde_json::to_value(tool.input_schema.as_ref())
        .unwrap_or_else(|_| serde_json::json!({
            "type": "object",
            "properties": {},
        }));
    serde_json::json!({
        "type": "function",
        "function": {
            "name": format!("mcp_{}_{}", server_name, tool.name),
            "description": tool.description.as_deref().unwrap_or(""),
            "parameters": params,
        }
    })
}

/// Extract tool call result content as string.
pub fn extract_result_text(result: &CallToolResult) -> String {
    use rmcp::model::RawContent;
    result
        .content
        .iter()
        .filter_map(|content| {
            // Content is Annotated<RawContent>, deref to get the inner
            match &content.raw {
                RawContent::Text(text) => Some(text.text.clone()),
                _ => None,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_serialization() {
        let config = McpServerConfig {
            name: "test".to_string(),
            transport: Transport::Stdio {
                command: vec!["echo".to_string()],
                args: vec!["hello".to_string()],
                env: HashMap::new(),
            },
            description: "Test server".to_string(),
            enabled: true,
        };

        let yaml = serde_yaml::to_string(&config).unwrap();
        assert!(yaml.contains("name: test"));
        assert!(yaml.contains("type: stdio"));

        let parsed: McpServerConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.name, "test");
    }

    #[test]
    fn manager_find_tool_when_empty() {
        let manager = McpClientManager::new();
        assert!(manager.find_tool("nonexistent").is_none());
    }

    #[test]
    fn mcp_tool_schema_conversion() {
        use std::sync::Arc;
        let schema_map: serde_json::Map<String, serde_json::Value> = serde_json::from_value(serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"}
            },
            "required": ["path"]
        })).unwrap();
        
        let tool = Tool::new(
            "read_file",
            "Read a file",
            Arc::new(schema_map),
        );

        let schema = mcp_tool_to_schema("filesystem", &tool);
        let func = schema["function"].as_object().unwrap();
        assert_eq!(func["name"], "mcp_filesystem_read_file");
        assert_eq!(func["description"], "Read a file");
    }

    #[test]
    fn config_defaults() {
        let yaml = r#"
name: test
transport:
  type: stdio
  command: ["echo"]
"#;
        let config: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.enabled);
        assert!(config.description.is_empty());
    }

    // ============================================================================
    // Integration Tests - MCP Configuration and Schema Handling
    // ============================================================================

    #[test]
    fn manager_operations() {
        let mut manager = McpClientManager::new();

        // Empty manager should have no servers
        assert!(manager.connected_servers().is_empty());
        assert!(manager.all_tools().is_empty());
        assert!(manager.find_tool("any_tool").is_none());

        // Disconnect non-existent server should return false
        assert!(!manager.disconnect("nonexistent"));
    }

    #[test]
    fn transport_stdio_config_parsing() {
        let yaml = r#"
name: filesystem
description: "Local filesystem access"
enabled: true
transport:
  type: stdio
  command: ["npx", "@modelcontextprotocol/server-filesystem"]
  args: ["--root", "/tmp"]
  env:
    DEBUG: "true"
    LOG_LEVEL: "info"
"#;
        let config: McpServerConfig = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(config.name, "filesystem");
        assert_eq!(config.description, "Local filesystem access");
        assert!(config.enabled);

        match config.transport {
            Transport::Stdio { command, args, env } => {
                assert_eq!(command, vec!["npx", "@modelcontextprotocol/server-filesystem"]);
                assert_eq!(args, vec!["--root", "/tmp"]);
                assert_eq!(env.get("DEBUG"), Some(&"true".to_string()));
                assert_eq!(env.get("LOG_LEVEL"), Some(&"info".to_string()));
            }
        }
    }

    #[test]
    fn transport_stdio_minimal_config() {
        let yaml = r#"
name: simple
transport:
  type: stdio
  command: ["python", "-m", "mcp_server"]
"#;
        let config: McpServerConfig = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(config.name, "simple");
        assert!(config.enabled); // default
        assert!(config.description.is_empty()); // default

        match config.transport {
            Transport::Stdio { command, args, env } => {
                assert_eq!(command, vec!["python", "-m", "mcp_server"]);
                assert!(args.is_empty()); // default
                assert!(env.is_empty()); // default
            }
        }
    }

    #[test]
    fn multiple_server_configs() {
        let yaml = r#"
mcp_servers:
  - name: filesystem
    transport:
      type: stdio
      command: ["npx", "@modelcontextprotocol/server-filesystem"]

  - name: github
    description: "GitHub operations"
    transport:
      type: stdio
      command: ["npx", "@modelcontextprotocol/server-github"]
      env:
        GITHUB_TOKEN: "${GITHUB_TOKEN}"

  - name: disabled
    enabled: false
    transport:
      type: stdio
      command: ["echo"]
"#;

        #[derive(serde::Deserialize)]
        struct ConfigList {
            mcp_servers: Vec<McpServerConfig>,
        }

        let configs: ConfigList = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(configs.mcp_servers.len(), 3);

        let fs = &configs.mcp_servers[0];
        assert_eq!(fs.name, "filesystem");
        assert!(fs.enabled);

        let gh = &configs.mcp_servers[1];
        assert_eq!(gh.name, "github");
        assert_eq!(gh.description, "GitHub operations");

        let disabled = &configs.mcp_servers[2];
        assert!(!disabled.enabled);
    }

    #[test]
    fn mcp_tool_to_schema_complex() {
        use std::sync::Arc;
        let schema_map: serde_json::Map<String, serde_json::Value> = serde_json::from_value(serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The file path to read"
                },
                "encoding": {
                    "type": "string",
                    "enum": ["utf8", "base64"],
                    "default": "utf8"
                }
            },
            "required": ["path"],
            "additionalProperties": false
        }))
        .unwrap();

        let tool = Tool::new(
            "read_file",
            "Read the contents of a file at the specified path",
            Arc::new(schema_map),
        );

        let schema = mcp_tool_to_schema("fs", &tool);

        // Verify structure
        assert!(schema.is_object());
        let func = schema["function"].as_object().unwrap();

        // Name should be prefixed
        assert_eq!(func["name"], "mcp_fs_read_file");

        // Description preserved
        assert_eq!(func["description"], "Read the contents of a file at the specified path");

        // Parameters schema preserved
        let params = func["parameters"].as_object().unwrap();
        assert_eq!(params["type"], "object");
        assert!(params.contains_key("properties"));
        assert!(params.contains_key("required"));
    }

    #[test]
    fn mcp_tool_to_schema_empty_schema() {
        use std::sync::Arc;
        let empty_schema: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();

        let tool = Tool::new("simple_action", "Performs a simple action", Arc::new(empty_schema));

        let schema = mcp_tool_to_schema("server", &tool);
        let func = schema["function"].as_object().unwrap();

        assert_eq!(func["name"], "mcp_server_simple_action");
        assert!(func["parameters"].as_object().unwrap().is_empty());
    }

    #[test]
    fn extract_result_text_multiple_contents() {
        use rmcp::model::{Content, RawContent};

        let result = CallToolResult::success(vec![
            Content::new(RawContent::text("Line 1"), None),
            Content::new(RawContent::text("Line 2"), None),
            Content::new(RawContent::text("Line 3"), None),
        ]);

        let text = extract_result_text(&result);
        assert_eq!(text, "Line 1\nLine 2\nLine 3");
    }

    #[test]
    fn extract_result_text_empty() {
        let result = CallToolResult::success(vec![]);

        let text = extract_result_text(&result);
        assert!(text.is_empty());
    }

    #[test]
    fn config_roundtrip_yaml() {
        let original = McpServerConfig {
            name: "test-server".to_string(),
            transport: Transport::Stdio {
                command: vec!["node".to_string(), "server.js".to_string()],
                args: vec!["--port".to_string(), "8080".to_string()],
                env: [
                    ("API_KEY".to_string(), "secret123".to_string()),
                    ("DEBUG".to_string(), "1".to_string()),
                ]
                .into_iter()
                .collect(),
            },
            description: "A test MCP server".to_string(),
            enabled: true,
        };

        let yaml = serde_yaml::to_string(&original).unwrap();
        let parsed: McpServerConfig = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(parsed.name, original.name);
        assert_eq!(parsed.description, original.description);
        assert_eq!(parsed.enabled, original.enabled);

        match (original.transport, parsed.transport) {
            (Transport::Stdio { command: c1, args: a1, env: e1 }, Transport::Stdio { command: c2, args: a2, env: e2 }) => {
                assert_eq!(c1, c2);
                assert_eq!(a1, a2);
                assert_eq!(e1, e2);
            }
        }
    }
}
