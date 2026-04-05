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
use std::time::Instant;

use rmcp::{
    ClientHandler, Peer, RoleClient,
    model::{CallToolRequestParams, CallToolResult, ReadResourceRequestParams, Resource, Tool},
    serve_client,
    service::ServiceError,
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
    /// HTTP SSE transport: communicates via Streamable HTTP (MCP 2025-03-26 spec).
    #[serde(alias = "sse", alias = "http")]
    Sse {
        /// Server URL (e.g., "http://localhost:8080/mcp" or "https://api.example.com/mcp").
        url: String,
        /// Optional bearer token for authentication.
        #[serde(default)]
        auth_token: Option<String>,
        /// Custom HTTP headers.
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    /// WebSocket transport: communicates over a persistent WebSocket connection.
    #[serde(alias = "websocket")]
    Ws {
        /// WebSocket URL (e.g., "ws://localhost:8080/mcp" or "wss://api.example.com/mcp").
        url: String,
        /// Optional bearer token for authentication (sent as Authorization header).
        #[serde(default)]
        auth_token: Option<String>,
        /// Optional extra headers for the WebSocket upgrade request.
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

/// Connection state for an MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    /// Not connected.
    Disconnected,
    /// Connection attempt in progress.
    Connecting,
    /// Fully connected and operational.
    Connected,
    /// Lost connection, attempting to reconnect.
    Reconnecting,
    /// Connection failed after all retries exhausted.
    Failed,
}

impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disconnected => write!(f, "disconnected"),
            Self::Connecting => write!(f, "connecting"),
            Self::Connected => write!(f, "connected"),
            Self::Reconnecting => write!(f, "reconnecting"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

/// Retry configuration for MCP server connections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    /// Maximum number of retry attempts (0 = no retries).
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Initial delay between retries in milliseconds.
    #[serde(default = "default_initial_delay_ms")]
    pub initial_delay_ms: u64,
    /// Maximum delay between retries in milliseconds.
    #[serde(default = "default_max_delay_ms")]
    pub max_delay_ms: u64,
}

fn default_max_retries() -> u32 {
    5
}
fn default_initial_delay_ms() -> u64 {
    1000
}
fn default_max_delay_ms() -> u64 {
    30_000
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            initial_delay_ms: default_initial_delay_ms(),
            max_delay_ms: default_max_delay_ms(),
        }
    }
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
    /// Retry configuration for connection attempts.
    #[serde(default)]
    pub retry: RetryConfig,
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
    /// When the connection was established.
    connected_at: Option<Instant>,
    /// Original config for reconnection.
    config: McpServerConfig,
}

impl McpConnection {
    /// Get the server name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get uptime since connection was established.
    pub fn uptime(&self) -> Option<std::time::Duration> {
        self.connected_at.map(|t| t.elapsed())
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

    /// List all resources from this server.
    pub async fn list_resources(&self) -> Result<Vec<Resource>, ServiceError> {
        self.peer.list_all_resources().await
    }

    /// Read a resource by URI, extracting text content.
    pub async fn read_resource(&self, uri: &str) -> Result<String, McpError> {
        let params = ReadResourceRequestParams::new(uri.to_string());
        let result = self
            .peer
            .read_resource(params)
            .await
            .map_err(McpError::Service)?;
        let text = result
            .contents
            .into_iter()
            .filter_map(|c| match c {
                rmcp::model::ResourceContents::TextResourceContents { text, .. } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(text)
    }

    /// Discover `skill://` resources and return (name, skill_md_content) pairs.
    pub async fn discover_skill_resources(&self) -> Vec<(String, String)> {
        let resources = match self.list_resources().await {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        let mut skills = Vec::new();
        for res in &resources {
            if res.raw.uri.starts_with("skill://") {
                if let Ok(content) = self.read_resource(&res.raw.uri).await {
                    if !content.is_empty() {
                        skills.push((res.raw.name.clone(), content));
                    }
                }
            }
        }
        skills
    }
}

/// MCP client manager for multiple server connections.
pub struct McpClientManager {
    /// Active connections indexed by server name.
    connections: HashMap<String, Arc<McpConnection>>,
    /// Connection state per server (tracks lifecycle across reconnects).
    states: HashMap<String, ConnectionState>,
}

impl Default for McpClientManager {
    fn default() -> Self {
        Self {
            connections: HashMap::new(),
            states: HashMap::new(),
        }
    }
}

impl McpClientManager {
    /// Create a new empty client manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Low-level connect (no skill discovery). Use `connect_and_discover_skills`
    /// for production code paths that should register `skill://` resources.
    async fn connect_internal(&mut self, config: McpServerConfig) -> Result<(), McpError> {
        if !config.enabled {
            return Ok(());
        }

        let name = config.name.clone();
        self.states
            .insert(name.clone(), ConnectionState::Connecting);
        match connect_to_server(config).await {
            Ok(connection) => {
                self.states.insert(name.clone(), ConnectionState::Connected);
                self.connections.insert(name, Arc::new(connection));
                Ok(())
            }
            Err(e) => {
                self.states.insert(name, ConnectionState::Failed);
                Err(e)
            }
        }
    }

    /// Connect to an MCP server and register any `skill://` resources into the
    /// unified skill registry. This is the primary entry point for establishing
    /// MCP connections — it ensures skill discovery always accompanies connection.
    pub async fn connect_and_discover_skills(
        &mut self,
        config: McpServerConfig,
        skill_registry: &astra_runtime::skills::UnifiedSkillRegistry,
    ) -> Result<usize, McpError> {
        let server_name = config.name.clone();
        self.connect_internal(config).await?;

        let conn = match self.connections.get(&server_name) {
            Some(c) => Arc::clone(c),
            None => return Ok(0),
        };

        let skill_resources = conn.discover_skill_resources().await;
        let mut registered = 0;
        for (_name, content) in &skill_resources {
            match skill_registry
                .register_mcp_skill(&server_name, content)
                .await
            {
                Ok(_) => registered += 1,
                Err(e) => {
                    eprintln!("  ⚠ Failed to register MCP skill from {server_name}: {e}");
                }
            }
        }
        Ok(registered)
    }

    /// Disconnect from an MCP server and remove its skills from the registry.
    /// This is the primary entry point for teardown — pairs with
    /// `connect_and_discover_skills`.
    pub async fn disconnect_and_remove_skills(
        &mut self,
        name: &str,
        skill_registry: &astra_runtime::skills::UnifiedSkillRegistry,
    ) -> bool {
        let removed = self.connections.remove(name).is_some();
        if removed {
            self.states
                .insert(name.to_string(), ConnectionState::Disconnected);
            let _ = skill_registry.remove_mcp_server_skills(name).await;
        }
        removed
    }

    /// Disconnect from an MCP server without registry cleanup.
    /// Prefer `disconnect_and_remove_skills` in production to keep skills in sync.
    pub fn disconnect(&mut self, name: &str) -> bool {
        let removed = self.connections.remove(name).is_some();
        if removed {
            self.states
                .insert(name.to_string(), ConnectionState::Disconnected);
        }
        removed
    }

    /// Get the connection state for a specific server.
    pub fn server_state(&self, name: &str) -> Option<ConnectionState> {
        self.states.get(name).copied()
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

    /// Get all MCP tool schemas suitable for LLM tool injection.
    /// Each schema follows the OpenAI function-calling format with `mcp_<server>_<name>` naming.
    pub fn all_tool_schemas(&self) -> Vec<serde_json::Value> {
        self.all_tools()
            .into_iter()
            .map(|(server, tool)| mcp_tool_to_schema(server, tool))
            .collect()
    }

    /// Find which server owns a sanitized MCP tool name (e.g. "mcp_fs_read_file").
    /// Returns (server_name, original_tool_name) if found.
    pub fn find_tool_by_mcp_name(&self, mcp_name: &str) -> Option<(&str, &str)> {
        for (server_name, conn) in &self.connections {
            for tool in &conn.tools {
                let sanitized = sanitize_tool_name(&format!("mcp_{}_{}", server_name, tool.name));
                if sanitized == mcp_name {
                    return Some((server_name, tool.name.as_ref()));
                }
            }
        }
        None
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

    /// Call a tool with automatic reconnect on transport failure.
    /// On first failure, reconnects to the server and retries the call once.
    pub async fn call_tool_with_reconnect(
        &mut self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult, McpError> {
        // First attempt
        let server_name = self
            .find_tool(tool_name)
            .map(|(name, _)| name.to_string())
            .ok_or_else(|| McpError::ToolNotFound(tool_name.to_string()))?;

        let first_result = {
            let conn = self
                .connections
                .get(&server_name)
                .ok_or_else(|| McpError::ServerNotConnected(server_name.clone()))?;
            conn.call_tool(tool_name, arguments.clone()).await
        };

        match first_result {
            Ok(result) => Ok(result),
            Err(e) => {
                eprintln!(
                    "  ↻ MCP tool '{}' failed on '{}': {e}, attempting reconnect…",
                    tool_name, server_name
                );

                // Try to reconnect
                match self.reconnect(&server_name).await {
                    Ok(tool_count) => {
                        eprintln!(
                            "  ✓ Reconnected to '{}' ({} tools), retrying…",
                            server_name, tool_count
                        );
                    }
                    Err(reconn_err) => {
                        return Err(McpError::ConnectionLost(
                            server_name,
                            format!("original error: {e}; reconnect failed: {reconn_err}"),
                        ));
                    }
                }

                // Retry the tool call
                let conn = self
                    .connections
                    .get(&server_name)
                    .ok_or_else(|| McpError::ServerNotConnected(server_name.clone()))?;
                conn.call_tool(tool_name, arguments)
                    .await
                    .map_err(McpError::Service)
            }
        }
    }

    /// Number of active connections.
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Get the connection state for all servers.
    pub fn server_states(&self) -> Vec<(&str, ConnectionState)> {
        self.states
            .iter()
            .map(|(name, state)| (name.as_str(), *state))
            .collect()
    }

    /// Reconnect a server using its stored config.
    /// Replaces the existing connection. Returns new tool count on success.
    pub async fn reconnect(&mut self, name: &str) -> Result<usize, McpError> {
        let config = match self.connections.get(name) {
            Some(conn) => conn.config.clone(),
            None => return Err(McpError::ServerNotConnected(name.to_string())),
        };

        // Remove old connection before reconnect attempt
        self.connections.remove(name);
        self.states
            .insert(name.to_string(), ConnectionState::Reconnecting);

        match connect_to_server(config).await {
            Ok(connection) => {
                let tool_count = connection.tools.len();
                self.states
                    .insert(name.to_string(), ConnectionState::Connected);
                self.connections
                    .insert(name.to_string(), Arc::new(connection));
                Ok(tool_count)
            }
            Err(e) => {
                self.states
                    .insert(name.to_string(), ConnectionState::Failed);
                Err(e)
            }
        }
    }

    /// Reconnect a server and re-discover skills. Primary reconnect entry point.
    pub async fn reconnect_and_rediscover_skills(
        &mut self,
        name: &str,
        skill_registry: &astra_runtime::skills::UnifiedSkillRegistry,
    ) -> Result<usize, McpError> {
        let config = match self.connections.get(name) {
            Some(conn) => conn.config.clone(),
            None => return Err(McpError::ServerNotConnected(name.to_string())),
        };

        // Clean up old skills first
        let _ = skill_registry.remove_mcp_server_skills(name).await;
        self.connections.remove(name);
        self.states
            .insert(name.to_string(), ConnectionState::Reconnecting);

        // Reconnect with retry
        let connection = match connect_to_server(config).await {
            Ok(conn) => conn,
            Err(e) => {
                self.states
                    .insert(name.to_string(), ConnectionState::Failed);
                return Err(e);
            }
        };
        self.states
            .insert(name.to_string(), ConnectionState::Connected);
        let conn = Arc::new(connection);
        self.connections.insert(name.to_string(), Arc::clone(&conn));

        // Re-discover skills
        let skill_resources = conn.discover_skill_resources().await;
        let mut registered = 0;
        for (_skill_name, content) in &skill_resources {
            match skill_registry.register_mcp_skill(name, content).await {
                Ok(_) => registered += 1,
                Err(e) => {
                    eprintln!("  ⚠ Failed to register MCP skill from {name}: {e}");
                }
            }
        }
        Ok(registered)
    }
}

/// Thread-safe MCP client manager.
pub type SharedMcpClientManager = Arc<RwLock<McpClientManager>>;

/// Create a new shared MCP client manager.
pub fn new_shared_manager() -> SharedMcpClientManager {
    Arc::new(RwLock::new(McpClientManager::new()))
}

/// Connect to an MCP server with exponential backoff retry.
async fn connect_to_server(config: McpServerConfig) -> Result<McpConnection, McpError> {
    let retry = config.retry.clone();
    let name = config.name.clone();

    let mut last_error = None;
    for attempt in 0..=retry.max_retries {
        if attempt > 0 {
            let delay_ms = std::cmp::min(
                retry.initial_delay_ms * 2u64.saturating_pow(attempt - 1),
                retry.max_delay_ms,
            );
            eprintln!(
                "  ↻ Retrying {name} (attempt {attempt}/{max}, backoff {delay_ms}ms)",
                max = retry.max_retries,
            );
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }

        match connect_once(&config).await {
            Ok(conn) => return Ok(conn),
            Err(e) => {
                // Don't retry config errors — they won't resolve themselves.
                if matches!(e, McpError::InvalidConfig(_)) {
                    return Err(e);
                }
                last_error = Some(e);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        McpError::Initialize(format!(
            "{name}: all {n} retries exhausted",
            n = retry.max_retries
        ))
    }))
}

/// Single connection attempt (no retry).
async fn connect_once(config: &McpServerConfig) -> Result<McpConnection, McpError> {
    match &config.transport {
        Transport::Stdio { command, args, env } => {
            connect_stdio(&config.name, command, args, env, config.clone()).await
        }
        Transport::Sse {
            url,
            auth_token,
            headers,
        } => {
            connect_sse(
                &config.name,
                url,
                auth_token.as_deref(),
                headers,
                config.clone(),
            )
            .await
        }
        Transport::Ws {
            url,
            auth_token,
            headers,
        } => {
            connect_ws(
                &config.name,
                url,
                auth_token.as_deref(),
                headers,
                config.clone(),
            )
            .await
        }
    }
}

/// Connect via stdio transport.
async fn connect_stdio(
    name: &str,
    command: &[String],
    args: &[String],
    env: &HashMap<String, String>,
    config: McpServerConfig,
) -> Result<McpConnection, McpError> {
    if command.is_empty() {
        return Err(McpError::InvalidConfig(
            "command cannot be empty".to_string(),
        ));
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
    let transport = TokioChildProcess::new(cmd).map_err(|e| McpError::Spawn(e.to_string()))?;

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
        connected_at: Some(Instant::now()),
        config,
    })
}

/// Connect via HTTP SSE (Streamable HTTP) transport.
async fn connect_sse(
    name: &str,
    url: &str,
    auth_token: Option<&str>,
    headers: &HashMap<String, String>,
    config: McpServerConfig,
) -> Result<McpConnection, McpError> {
    use rmcp::transport::streamable_http_client::{
        StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
    };

    if url.is_empty() {
        return Err(McpError::InvalidConfig("url cannot be empty".to_string()));
    }

    // Build transport config
    let mut transport_config = StreamableHttpClientTransportConfig::with_uri(url);
    transport_config.reinit_on_expired_session = true;

    if let Some(token) = auth_token {
        transport_config = transport_config.auth_header(token);
    }

    if !headers.is_empty() {
        let mut custom = HashMap::new();
        for (k, v) in headers {
            let header_name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| McpError::InvalidConfig(format!("invalid header name '{k}': {e}")))?;
            let header_value = reqwest::header::HeaderValue::from_str(v).map_err(|e| {
                McpError::InvalidConfig(format!("invalid header value for '{k}': {e}"))
            })?;
            custom.insert(header_name, header_value);
        }
        transport_config = transport_config.custom_headers(custom);
    }

    // Create transport (reqwest-based, via rmcp feature)
    let transport = StreamableHttpClientTransport::from_config(transport_config);

    // Connect as MCP client
    let running = serve_client(NoOpClientHandler, transport)
        .await
        .map_err(|e| McpError::Initialize(format!("SSE connect to {url}: {e}")))?;

    let peer = running.peer().clone();

    // Fetch initial tool list
    let tools = peer.list_all_tools().await.map_err(McpError::Service)?;

    Ok(McpConnection {
        name: name.to_string(),
        peer,
        tools,
        connected_at: Some(Instant::now()),
        config,
    })
}

/// Connect via WebSocket transport.
///
/// Uses `(AsyncRead, AsyncWrite)` adapter: spawns bridge tasks that convert
/// between WebSocket text frames and newline-delimited JSON bytes, then lets
/// rmcp's `transport-async-rw` handle the JSON-RPC framing.
async fn connect_ws(
    name: &str,
    url: &str,
    auth_token: Option<&str>,
    headers: &HashMap<String, String>,
    config: McpServerConfig,
) -> Result<McpConnection, McpError> {
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio_tungstenite::tungstenite;

    if url.is_empty() {
        return Err(McpError::InvalidConfig("url cannot be empty".to_string()));
    }

    // Build WebSocket request with optional auth and custom headers
    let mut request = tungstenite::http::Request::builder()
        .uri(url)
        .header("Sec-WebSocket-Version", "13")
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header(
            "Sec-WebSocket-Key",
            tungstenite::handshake::client::generate_key(),
        );
    if let Some(token) = auth_token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    for (key, value) in headers {
        request = request.header(key.as_str(), value.as_str());
    }
    let request = request
        .body(())
        .map_err(|e| McpError::InvalidConfig(format!("invalid WebSocket request: {e}")))?;

    let (ws_stream, _response) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| McpError::Initialize(format!("WebSocket connect to {url}: {e}")))?;

    let (mut ws_sink, mut ws_read) = ws_stream.split();

    // Create DuplexStream pairs to bridge WebSocket ↔ rmcp byte streams.
    // rmcp's transport-async-rw reads newline-delimited JSON from AsyncRead
    // and writes newline-delimited JSON to AsyncWrite.
    let (rmcp_read, mut bridge_write) = tokio::io::duplex(64 * 1024);
    let (mut bridge_read, rmcp_write) = tokio::io::duplex(64 * 1024);

    let ws_name = name.to_string();

    // Bridge: WebSocket text frames → bytes for rmcp to read
    let reader_name = ws_name.clone();
    tokio::spawn(async move {
        loop {
            match ws_read.next().await {
                Some(Ok(tungstenite::Message::Text(text))) => {
                    if bridge_write.write_all(text.as_bytes()).await.is_err()
                        || bridge_write.write_all(b"\n").await.is_err()
                    {
                        break;
                    }
                }
                Some(Ok(tungstenite::Message::Close(_))) => break,
                Some(Ok(_)) => {} // ignore binary, ping, pong
                Some(Err(e)) => {
                    eprintln!("  ⚠ MCP WebSocket read error [{reader_name}]: {e}");
                    break;
                }
                None => break,
            }
        }
        drop(bridge_write); // signal EOF to rmcp reader
    });

    // Bridge: rmcp writes → WebSocket text frames
    let writer_name = ws_name;
    tokio::spawn(async move {
        let mut reader = BufReader::new(&mut bridge_read);
        let mut line = String::new();
        loop {
            match reader.read_line(&mut line).await {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let trimmed = line.trim_end();
                    if !trimmed.is_empty() {
                        if ws_sink
                            .send(tungstenite::Message::Text(trimmed.to_owned().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    line.clear();
                }
                Err(e) => {
                    eprintln!("  ⚠ MCP WebSocket write-bridge error [{writer_name}]: {e}");
                    break;
                }
            }
        }
    });

    // Connect as MCP client using (AsyncRead, AsyncWrite)
    let running = serve_client(NoOpClientHandler, (rmcp_read, rmcp_write))
        .await
        .map_err(|e| McpError::Initialize(format!("MCP init over WebSocket {url}: {e}")))?;

    let peer = running.peer().clone();
    let tools = peer.list_all_tools().await.map_err(McpError::Service)?;

    Ok(McpConnection {
        name: name.to_string(),
        peer,
        tools,
        connected_at: Some(Instant::now()),
        config,
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

    #[error("Connection lost to server {0}: {1}")]
    ConnectionLost(String, String),

    #[error("Reconnection failed for server {0} after {1} attempts")]
    ReconnectionFailed(String, u32),
}

/// Maximum length for tool descriptions sent to the model.
/// Matches Claude Code's MAX_MCP_DESCRIPTION_LENGTH.
pub const MAX_DESCRIPTION_LENGTH: usize = 2048;

/// Maximum character length for tool call result content.
/// ~25K tokens × 4 chars/token = 100K chars.
pub const MAX_RESULT_CONTENT_LENGTH: usize = 100_000;

/// Truncate a string to `max_len` chars, appending a marker if truncated.
const TRUNCATION_MARKER: &str = "… [truncated]";

fn truncate_with_marker(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    // Reserve space for the marker so total output stays within max_len
    let content_budget = max_len.saturating_sub(TRUNCATION_MARKER.len());
    let end = s.floor_char_boundary(content_budget);
    format!("{}{TRUNCATION_MARKER}", &s[..end])
}

/// Sanitize a tool name: only alphanumeric, underscore, hyphen allowed.
/// Replaces invalid chars with underscore.
pub fn sanitize_tool_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Convert MCP Tool to astra tool schema format.
/// Applies description truncation and name sanitization.
pub fn mcp_tool_to_schema(server_name: &str, tool: &Tool) -> serde_json::Value {
    // input_schema is Arc<JsonObject>, convert to Value
    let params = serde_json::to_value(tool.input_schema.as_ref()).unwrap_or_else(|_| {
        serde_json::json!({
            "type": "object",
            "properties": {},
        })
    });

    let raw_desc = tool.description.as_deref().unwrap_or("");
    let description = truncate_with_marker(raw_desc, MAX_DESCRIPTION_LENGTH);
    let tool_name = sanitize_tool_name(&format!("mcp_{}_{}", server_name, tool.name));

    serde_json::json!({
        "type": "function",
        "function": {
            "name": tool_name,
            "description": description,
            "parameters": params,
        }
    })
}

/// Extract tool call result content as string, with truncation.
pub fn extract_result_text(result: &CallToolResult) -> String {
    extract_result_text_with_limit(result, MAX_RESULT_CONTENT_LENGTH)
}

/// Extract tool call result content as string, truncated to `max_len` chars.
pub fn extract_result_text_with_limit(result: &CallToolResult, max_len: usize) -> String {
    use rmcp::model::RawContent;

    let mut parts = Vec::new();
    let mut total_len = 0;

    for content in &result.content {
        match &content.raw {
            RawContent::Text(text) => {
                let remaining = max_len.saturating_sub(total_len);
                if remaining == 0 {
                    break;
                }
                if text.text.len() <= remaining {
                    total_len += text.text.len();
                    parts.push(text.text.clone());
                } else {
                    let end = text.text.floor_char_boundary(remaining);
                    parts.push(text.text[..end].to_string());
                    total_len += end;
                    break;
                }
            }
            _ => {}
        }
    }

    let joined = parts.join("\n");
    if total_len >= max_len {
        format!(
            "{}\n\n[OUTPUT TRUNCATED - exceeded {} char limit]",
            joined, max_len
        )
    } else {
        joined
    }
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
            retry: RetryConfig::default(),
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
        let schema_map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"]
            }))
            .unwrap();

        let tool = Tool::new("read_file", "Read a file", Arc::new(schema_map));

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
                assert_eq!(
                    command,
                    vec!["npx", "@modelcontextprotocol/server-filesystem"]
                );
                assert_eq!(args, vec!["--root", "/tmp"]);
                assert_eq!(env.get("DEBUG"), Some(&"true".to_string()));
                assert_eq!(env.get("LOG_LEVEL"), Some(&"info".to_string()));
            }
            _ => panic!("expected Stdio transport"),
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
            _ => panic!("expected Stdio transport"),
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
        let schema_map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({
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
        assert_eq!(
            func["description"],
            "Read the contents of a file at the specified path"
        );

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

        let tool = Tool::new(
            "simple_action",
            "Performs a simple action",
            Arc::new(empty_schema),
        );

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
            retry: RetryConfig {
                max_retries: 3,
                initial_delay_ms: 500,
                max_delay_ms: 10_000,
            },
        };

        let yaml = serde_yaml::to_string(&original).unwrap();
        let parsed: McpServerConfig = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(parsed.name, original.name);
        assert_eq!(parsed.description, original.description);
        assert_eq!(parsed.enabled, original.enabled);

        match (original.transport, parsed.transport) {
            (
                Transport::Stdio {
                    command: c1,
                    args: a1,
                    env: e1,
                },
                Transport::Stdio {
                    command: c2,
                    args: a2,
                    env: e2,
                },
            ) => {
                assert_eq!(c1, c2);
                assert_eq!(a1, a2);
                assert_eq!(e1, e2);
            }
            _ => panic!("expected matching Stdio transports"),
        }
    }

    // ============================================================================
    // Connection State & Retry Tests
    // ============================================================================

    #[test]
    fn connection_state_display() {
        assert_eq!(ConnectionState::Disconnected.to_string(), "disconnected");
        assert_eq!(ConnectionState::Connecting.to_string(), "connecting");
        assert_eq!(ConnectionState::Connected.to_string(), "connected");
        assert_eq!(ConnectionState::Reconnecting.to_string(), "reconnecting");
        assert_eq!(ConnectionState::Failed.to_string(), "failed");
    }

    #[test]
    fn retry_config_defaults() {
        let config = RetryConfig::default();
        assert_eq!(config.max_retries, 5);
        assert_eq!(config.initial_delay_ms, 1000);
        assert_eq!(config.max_delay_ms, 30_000);
    }

    #[test]
    fn retry_config_from_yaml() {
        let yaml = r#"
name: test
transport:
  type: stdio
  command: ["echo"]
retry:
  max_retries: 3
  initial_delay_ms: 500
  max_delay_ms: 10000
"#;
        let config: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.retry.max_retries, 3);
        assert_eq!(config.retry.initial_delay_ms, 500);
        assert_eq!(config.retry.max_delay_ms, 10_000);
    }

    #[test]
    fn retry_config_defaults_when_omitted() {
        let yaml = r#"
name: test
transport:
  type: stdio
  command: ["echo"]
"#;
        let config: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.retry.max_retries, 5);
        assert_eq!(config.retry.initial_delay_ms, 1000);
        assert_eq!(config.retry.max_delay_ms, 30_000);
    }

    #[test]
    fn exponential_backoff_calculation() {
        let retry = RetryConfig {
            max_retries: 5,
            initial_delay_ms: 1000,
            max_delay_ms: 30_000,
        };

        let delays: Vec<u64> = (1..=5)
            .map(|attempt| {
                std::cmp::min(
                    retry.initial_delay_ms * 2u64.saturating_pow(attempt - 1),
                    retry.max_delay_ms,
                )
            })
            .collect();

        assert_eq!(delays, vec![1000, 2000, 4000, 8000, 16000]);
    }

    #[test]
    fn exponential_backoff_caps_at_max() {
        let retry = RetryConfig {
            max_retries: 10,
            initial_delay_ms: 1000,
            max_delay_ms: 5000,
        };

        let delay_at_10 = std::cmp::min(
            retry.initial_delay_ms * 2u64.saturating_pow(9),
            retry.max_delay_ms,
        );
        assert_eq!(delay_at_10, 5000);
    }

    #[test]
    fn server_states_empty_manager() {
        let manager = McpClientManager::new();
        assert!(manager.server_states().is_empty());
    }

    #[test]
    fn state_tracking_in_manager() {
        let mut manager = McpClientManager::new();

        // Initially no states
        assert!(manager.server_state("test").is_none());

        // Simulate connect lifecycle (normally via connect_internal)
        manager
            .states
            .insert("test".into(), ConnectionState::Connecting);
        assert_eq!(
            manager.server_state("test"),
            Some(ConnectionState::Connecting)
        );

        // Simulate failure
        manager
            .states
            .insert("test".into(), ConnectionState::Failed);
        assert_eq!(manager.server_state("test"), Some(ConnectionState::Failed));

        // Simulate reconnecting
        manager
            .states
            .insert("test".into(), ConnectionState::Reconnecting);
        assert_eq!(
            manager.server_state("test"),
            Some(ConnectionState::Reconnecting)
        );

        // Simulate connected (then disconnect)
        manager
            .states
            .insert("test".into(), ConnectionState::Connected);
        assert_eq!(
            manager.server_state("test"),
            Some(ConnectionState::Connected)
        );

        // disconnect() without an actual connection doesn't change state
        // (returns false since no connection exists)
        assert!(!manager.disconnect("test"));
        // State stays Connected since disconnect only updates on actual removal
        assert_eq!(
            manager.server_state("test"),
            Some(ConnectionState::Connected)
        );

        // server_states includes all tracked servers
        let states = manager.server_states();
        assert_eq!(states.len(), 1);
    }

    #[test]
    fn error_variants() {
        let e = McpError::ConnectionLost("server1".into(), "broken pipe".into());
        assert!(e.to_string().contains("server1"));
        assert!(e.to_string().contains("broken pipe"));

        let e = McpError::ReconnectionFailed("server2".into(), 5);
        assert!(e.to_string().contains("server2"));
        assert!(e.to_string().contains("5"));
    }

    // ============================================================================
    // Tool Validation & Truncation Tests
    // ============================================================================

    #[test]
    fn sanitize_tool_name_valid() {
        assert_eq!(sanitize_tool_name("read_file"), "read_file");
        assert_eq!(sanitize_tool_name("mcp_fs_read-file"), "mcp_fs_read-file");
        assert_eq!(sanitize_tool_name("abc123"), "abc123");
    }

    #[test]
    fn sanitize_tool_name_special_chars() {
        assert_eq!(sanitize_tool_name("read file"), "read_file");
        assert_eq!(sanitize_tool_name("tool.name"), "tool_name");
        assert_eq!(sanitize_tool_name("ns::func"), "ns__func");
        assert_eq!(sanitize_tool_name("path/to/tool"), "path_to_tool");
    }

    #[test]
    fn truncate_with_marker_short() {
        assert_eq!(truncate_with_marker("hello", 10), "hello");
        assert_eq!(truncate_with_marker("hello", 5), "hello");
    }

    #[test]
    fn truncate_with_marker_long() {
        let long = "a".repeat(3000);
        let result = truncate_with_marker(&long, MAX_DESCRIPTION_LENGTH);
        assert!(
            result.len() <= MAX_DESCRIPTION_LENGTH,
            "truncated output should not exceed max_len"
        );
        assert!(result.ends_with("… [truncated]"));
    }

    #[test]
    fn mcp_tool_to_schema_truncates_description() {
        use std::sync::Arc;
        let empty_schema: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        let long_desc = "x".repeat(5000);

        let tool = Tool::new("test_tool", long_desc, Arc::new(empty_schema));
        let schema = mcp_tool_to_schema("server", &tool);
        let desc = schema["function"]["description"].as_str().unwrap();

        assert!(
            desc.len() <= MAX_DESCRIPTION_LENGTH,
            "truncated desc should not exceed max"
        );
        assert!(desc.ends_with("… [truncated]"));
    }

    #[test]
    fn mcp_tool_to_schema_sanitizes_name() {
        use std::sync::Arc;
        let empty_schema: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        let tool = Tool::new(
            "read file".to_string(),
            "desc".to_string(),
            Arc::new(empty_schema),
        );
        let schema = mcp_tool_to_schema("my.server", &tool);

        let name = schema["function"]["name"].as_str().unwrap();
        assert_eq!(name, "mcp_my_server_read_file");
    }

    #[test]
    fn extract_result_text_truncation() {
        use rmcp::model::{Content, RawContent};

        let big_text = "a".repeat(200);
        let result = CallToolResult::success(vec![
            Content::new(RawContent::text(&big_text), None),
            Content::new(RawContent::text(&big_text), None),
        ]);

        // With a small limit
        let text = extract_result_text_with_limit(&result, 250);
        assert!(text.contains("[OUTPUT TRUNCATED"));
        assert!(text.len() < 500);
    }

    #[test]
    fn extract_result_text_no_truncation_when_small() {
        use rmcp::model::{Content, RawContent};

        let result = CallToolResult::success(vec![
            Content::new(RawContent::text("hello"), None),
            Content::new(RawContent::text("world"), None),
        ]);

        let text = extract_result_text_with_limit(&result, 1000);
        assert_eq!(text, "hello\nworld");
        assert!(!text.contains("[OUTPUT TRUNCATED"));
    }

    // ============================================================================
    // SSE Transport Tests
    // ============================================================================

    #[test]
    fn sse_transport_config_parsing() {
        let yaml = r#"
name: remote-server
transport:
  type: sse
  url: "https://api.example.com/mcp"
  auth_token: "my-token"
  headers:
    X-Api-Key: "abc123"
"#;
        let config: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.name, "remote-server");
        match &config.transport {
            Transport::Sse {
                url,
                auth_token,
                headers,
            } => {
                assert_eq!(url, "https://api.example.com/mcp");
                assert_eq!(auth_token.as_deref(), Some("my-token"));
                assert_eq!(headers.get("X-Api-Key"), Some(&"abc123".to_string()));
            }
            _ => panic!("expected SSE transport"),
        }
    }

    #[test]
    fn sse_transport_http_alias() {
        let yaml = r#"
name: http-server
transport:
  type: http
  url: "http://localhost:8080/mcp"
"#;
        let config: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        match &config.transport {
            Transport::Sse {
                url,
                auth_token,
                headers,
            } => {
                assert_eq!(url, "http://localhost:8080/mcp");
                assert!(auth_token.is_none());
                assert!(headers.is_empty());
            }
            _ => panic!("expected SSE transport"),
        }
    }

    #[test]
    fn sse_transport_minimal() {
        let yaml = r#"
name: simple-sse
transport:
  type: sse
  url: "http://localhost:3000"
"#;
        let config: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        match &config.transport {
            Transport::Sse {
                url,
                auth_token,
                headers,
            } => {
                assert_eq!(url, "http://localhost:3000");
                assert!(auth_token.is_none());
                assert!(headers.is_empty());
            }
            _ => panic!("expected SSE transport"),
        }
    }

    #[test]
    fn mixed_transport_configs() {
        let yaml = r#"
mcp_servers:
  - name: local
    transport:
      type: stdio
      command: ["echo"]
  - name: remote
    transport:
      type: sse
      url: "https://api.example.com/mcp"
      auth_token: "token123"
"#;

        #[derive(serde::Deserialize)]
        struct ConfigList {
            mcp_servers: Vec<McpServerConfig>,
        }

        let configs: ConfigList = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(configs.mcp_servers.len(), 2);

        assert!(matches!(
            configs.mcp_servers[0].transport,
            Transport::Stdio { .. }
        ));
        assert!(matches!(
            configs.mcp_servers[1].transport,
            Transport::Sse { .. }
        ));
    }

    #[test]
    fn ws_transport_config_parsing() {
        let yaml = r#"
name: ws-server
transport:
  type: ws
  url: "wss://api.example.com/mcp"
  auth_token: "ws-token"
"#;
        let config: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.name, "ws-server");
        match &config.transport {
            Transport::Ws {
                url, auth_token, ..
            } => {
                assert_eq!(url, "wss://api.example.com/mcp");
                assert_eq!(auth_token.as_deref(), Some("ws-token"));
            }
            _ => panic!("expected Ws transport"),
        }
    }

    #[test]
    fn ws_transport_websocket_alias() {
        let yaml = r#"
name: ws-alias
transport:
  type: websocket
  url: "ws://localhost:9090/mcp"
"#;
        let config: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        match &config.transport {
            Transport::Ws {
                url, auth_token, ..
            } => {
                assert_eq!(url, "ws://localhost:9090/mcp");
                assert!(auth_token.is_none());
            }
            _ => panic!("expected Ws transport"),
        }
    }

    #[test]
    fn ws_transport_minimal() {
        let yaml = r#"
name: simple-ws
transport:
  type: ws
  url: "ws://localhost:3000"
"#;
        let config: McpServerConfig = serde_yaml::from_str(yaml).unwrap();
        match &config.transport {
            Transport::Ws {
                url, auth_token, ..
            } => {
                assert_eq!(url, "ws://localhost:3000");
                assert!(auth_token.is_none());
            }
            _ => panic!("expected Ws transport"),
        }
    }

    #[test]
    fn mixed_transport_all_three() {
        let yaml = r#"
mcp_servers:
  - name: local
    transport:
      type: stdio
      command: ["echo"]
  - name: remote-sse
    transport:
      type: sse
      url: "https://api.example.com/mcp"
  - name: remote-ws
    transport:
      type: ws
      url: "wss://api.example.com/ws"
"#;

        #[derive(serde::Deserialize)]
        struct ConfigList {
            mcp_servers: Vec<McpServerConfig>,
        }

        let configs: ConfigList = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(configs.mcp_servers.len(), 3);
        assert!(matches!(
            configs.mcp_servers[0].transport,
            Transport::Stdio { .. }
        ));
        assert!(matches!(
            configs.mcp_servers[1].transport,
            Transport::Sse { .. }
        ));
        assert!(matches!(
            configs.mcp_servers[2].transport,
            Transport::Ws { .. }
        ));
    }
}
