//! MCP (Model Context Protocol) client for external tool integration.
//!
//! This module provides a client implementation for connecting to MCP servers,
//! allowing astra to leverage external tools and resources exposed via MCP.
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use rmcp::{
    ClientHandler, Peer, RoleClient,
    model::{
        CallToolRequestParams, CallToolResult, CreateMessageRequestParams, CreateMessageResult,
        ErrorData as McpHandlerError, GetPromptRequestParams, GetPromptResult, LoggingLevel,
        Prompt, ReadResourceRequestParams, Resource, Role, SamplingMessage,
        SamplingMessageContent, SetLevelRequestParams, SubscribeRequestParams, Tool,
        UnsubscribeRequestParams,
    },
    serve_client,
    service::{NotificationContext, RequestContext, ServiceError},
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

/// Configuration for MCP sampling — allows the handler to forward
/// `sampling/createMessage` requests to our LLM API.
#[derive(Clone)]
pub struct SamplingConfig {
    pub api: Arc<astra_thin_client::ThinClient>,
    pub token: String,
    pub model: String,
}

impl std::fmt::Debug for SamplingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SamplingConfig")
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

/// MCP client handler that tracks tool list change notifications.
///
/// When the server sends change notifications for tools, prompts, or resources,
/// the handler sets the corresponding flag. The connection owner can poll these
/// flags and refresh as needed.
///
/// Optionally holds a [`SamplingConfig`] to fulfill `sampling/createMessage`
/// requests from MCP servers.
#[derive(Debug, Clone)]
struct ChangeHandler {
    tools_changed: Arc<AtomicBool>,
    prompts_changed: Arc<AtomicBool>,
    resources_changed: Arc<AtomicBool>,
    sampling: Option<Arc<SamplingConfig>>,
}

impl ChangeHandler {
    fn new(
        sampling: Option<Arc<SamplingConfig>>,
    ) -> (Self, Arc<AtomicBool>, Arc<AtomicBool>, Arc<AtomicBool>) {
        let tools = Arc::new(AtomicBool::new(false));
        let prompts = Arc::new(AtomicBool::new(false));
        let resources = Arc::new(AtomicBool::new(false));
        (
            Self {
                tools_changed: tools.clone(),
                prompts_changed: prompts.clone(),
                resources_changed: resources.clone(),
                sampling,
            },
            tools,
            prompts,
            resources,
        )
    }
}

impl ClientHandler for ChangeHandler {
    fn on_tool_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        self.tools_changed.store(true, Ordering::Release);
        std::future::ready(())
    }

    fn on_prompt_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        self.prompts_changed.store(true, Ordering::Release);
        std::future::ready(())
    }

    fn on_resource_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        self.resources_changed.store(true, Ordering::Release);
        std::future::ready(())
    }

    fn on_resource_updated(
        &self,
        _params: rmcp::model::ResourceUpdatedNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        self.resources_changed.store(true, Ordering::Release);
        std::future::ready(())
    }

    fn on_logging_message(
        &self,
        params: rmcp::model::LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let level = match params.level {
            rmcp::model::LoggingLevel::Debug => "DEBUG",
            rmcp::model::LoggingLevel::Info => "INFO",
            rmcp::model::LoggingLevel::Notice => "NOTICE",
            rmcp::model::LoggingLevel::Warning => "WARN",
            rmcp::model::LoggingLevel::Error => "ERROR",
            rmcp::model::LoggingLevel::Critical => "CRIT",
            rmcp::model::LoggingLevel::Alert => "ALERT",
            rmcp::model::LoggingLevel::Emergency => "EMERG",
        };
        let logger = params.logger.as_deref().unwrap_or("mcp");
        let data = if let Some(s) = params.data.as_str() {
            s.to_string()
        } else {
            params.data.to_string()
        };
        eprintln!("  [{level}] {logger}: {data}");
        std::future::ready(())
    }

    fn create_message(
        &self,
        params: CreateMessageRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = Result<CreateMessageResult, McpHandlerError>> + Send + '_ {
        let sampling = self.sampling.clone();
        async move {
            let config = sampling.ok_or_else(|| {
                McpHandlerError::method_not_found::<rmcp::model::CreateMessageRequestMethod>()
            })?;
            do_sampling(&config, params).await
        }
    }
}

// ── Sampling implementation ──────────────────────────────────────────────

/// Convert MCP `SamplingMessage` list to OpenAI `messages` array.
fn sampling_messages_to_openai(messages: &[SamplingMessage]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .map(|msg| {
            let role = match msg.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };

            let contents: Vec<serde_json::Value> = msg
                .content
                .iter()
                .filter_map(|c| match c {
                    SamplingMessageContent::Text(t) => Some(serde_json::json!({
                        "type": "text",
                        "text": t.text,
                    })),
                    SamplingMessageContent::Image(img) => Some(serde_json::json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{};base64,{}", img.mime_type, img.data),
                        },
                    })),
                    _ => None, // ToolUse/ToolResult/Audio — not mapped yet
                })
                .collect();

            if contents.len() == 1 {
                if let Some(text) = contents[0].get("text") {
                    return serde_json::json!({ "role": role, "content": text });
                }
            }
            serde_json::json!({ "role": role, "content": contents })
        })
        .collect()
}

/// Fulfill a sampling request by calling our LLM API.
async fn do_sampling(
    config: &SamplingConfig,
    params: CreateMessageRequestParams,
) -> Result<CreateMessageResult, McpHandlerError> {
    let mut body = serde_json::json!({
        "model": config.model,
        "messages": sampling_messages_to_openai(&params.messages),
        "max_tokens": params.max_tokens,
    });

    if let Some(system) = &params.system_prompt {
        if let Some(arr) = body["messages"].as_array_mut() {
            arr.insert(0, serde_json::json!({ "role": "system", "content": system }));
        }
    }
    if let Some(temp) = params.temperature {
        body["temperature"] = serde_json::json!(temp);
    }
    if let Some(stops) = &params.stop_sequences {
        body["stop"] = serde_json::json!(stops);
    }

    let resp = config
        .api
        .post_completions(&config.token, &body)
        .await
        .map_err(|e| McpHandlerError::internal_error(format!("LLM API error: {e}"), None))?;

    let choice = resp["choices"]
        .as_array()
        .and_then(|a| a.first())
        .ok_or_else(|| McpHandlerError::internal_error("no choices in LLM response", None))?;

    let text = choice["message"]["content"]
        .as_str()
        .unwrap_or("");

    let finish_reason = choice["finish_reason"]
        .as_str()
        .unwrap_or("endTurn");

    let stop_reason = match finish_reason {
        "stop" => CreateMessageResult::STOP_REASON_END_TURN,
        "length" => CreateMessageResult::STOP_REASON_END_MAX_TOKEN,
        "tool_calls" => CreateMessageResult::STOP_REASON_TOOL_USE,
        other => other,
    };

    let model_name = resp["model"]
        .as_str()
        .unwrap_or(&config.model);

    Ok(CreateMessageResult::new(
        SamplingMessage::assistant_text(text),
        model_name.to_string(),
    )
    .with_stop_reason(stop_reason))
}

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
    /// Flag set by the notification handler when the server's tool list changes.
    tools_changed: Arc<AtomicBool>,
    /// Flag set by the notification handler when the server's prompt list changes.
    prompts_changed: Arc<AtomicBool>,
    /// Flag set by the notification handler when the server's resource list changes.
    resources_changed: Arc<AtomicBool>,
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
        self.tools_changed.store(false, Ordering::Release);
        Ok(&self.tools)
    }

    /// Check if the server signalled a tool list change and refresh if so.
    ///
    /// Returns `true` if tools were refreshed, `false` if no change detected.
    pub async fn refresh_tools_if_changed(&mut self) -> Result<bool, ServiceError> {
        if self.tools_changed.swap(false, Ordering::AcqRel) {
            self.tools = self.peer.list_all_tools().await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Whether the server has signalled a tool list change that hasn't been
    /// consumed yet.
    pub fn has_pending_tool_change(&self) -> bool {
        self.tools_changed.load(Ordering::Acquire)
    }

    /// Whether the server has signalled a prompt list change that hasn't been
    /// consumed yet.
    pub fn has_pending_prompt_change(&self) -> bool {
        self.prompts_changed.load(Ordering::Acquire)
    }

    /// Consume the prompts_changed flag (returns previous value).
    pub fn consume_prompt_change(&self) -> bool {
        self.prompts_changed.swap(false, Ordering::AcqRel)
    }

    /// Whether the server has signalled a resource list/update change.
    pub fn has_pending_resource_change(&self) -> bool {
        self.resources_changed.load(Ordering::Acquire)
    }

    /// Consume the resources_changed flag (returns previous value).
    pub fn consume_resource_change(&self) -> bool {
        self.resources_changed.swap(false, Ordering::AcqRel)
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

    /// Subscribe to updates for a specific resource URI.
    pub async fn subscribe_resource(&self, uri: &str) -> Result<(), ServiceError> {
        self.peer
            .subscribe(SubscribeRequestParams::new(uri))
            .await
    }

    /// Unsubscribe from updates for a specific resource URI.
    pub async fn unsubscribe_resource(&self, uri: &str) -> Result<(), ServiceError> {
        self.peer
            .unsubscribe(UnsubscribeRequestParams::new(uri))
            .await
    }

    /// Set the logging level for this server.
    pub async fn set_log_level(&self, level: LoggingLevel) -> Result<(), ServiceError> {
        self.peer
            .set_level(SetLevelRequestParams::new(level))
            .await
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

    /// List all prompts available from this server.
    pub async fn list_prompts(&self) -> Result<Vec<Prompt>, ServiceError> {
        self.peer.list_all_prompts().await
    }

    /// Get a specific prompt by name, optionally with arguments.
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<GetPromptResult, ServiceError> {
        let mut params = GetPromptRequestParams::new(name.to_string());
        if let Some(args) = arguments {
            params.arguments = Some(args);
        }
        self.peer.get_prompt(params).await
    }
}

/// MCP client manager for multiple server connections.
pub struct McpClientManager {
    /// Active connections indexed by server name.
    connections: HashMap<String, Arc<McpConnection>>,
    /// Connection state per server (tracks lifecycle across reconnects).
    states: HashMap<String, ConnectionState>,
    /// Optional sampling config — forwarded to each new connection's handler.
    sampling: Option<Arc<SamplingConfig>>,
}

impl Default for McpClientManager {
    fn default() -> Self {
        Self {
            connections: HashMap::new(),
            states: HashMap::new(),
            sampling: None,
        }
    }
}

impl McpClientManager {
    /// Create a new empty client manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set (or clear) the sampling configuration. All subsequent connections
    /// will use this config to handle `sampling/createMessage` requests.
    pub fn set_sampling_config(&mut self, config: Option<SamplingConfig>) {
        self.sampling = config.map(Arc::new);
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
        match connect_to_server(config, self.sampling.clone()).await {
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

    /// List all prompts from all connected servers.
    /// Returns (server_name, prompt) pairs.
    pub async fn all_prompts(&self) -> Vec<(String, Prompt)> {
        let mut result = Vec::new();
        for (name, conn) in &self.connections {
            match conn.list_prompts().await {
                Ok(prompts) => {
                    for p in prompts {
                        result.push((name.clone(), p));
                    }
                }
                Err(e) => {
                    eprintln!("  ⚠ Failed to list prompts from {name}: {e}");
                }
            }
        }
        result
    }

    /// List all resources from all connected servers.
    /// Returns (server_name, resource) pairs.
    pub async fn all_resources(&self) -> Vec<(String, Resource)> {
        let mut result = Vec::new();
        for (name, conn) in &self.connections {
            match conn.list_resources().await {
                Ok(resources) => {
                    for r in resources {
                        result.push((name.clone(), r));
                    }
                }
                Err(e) => {
                    eprintln!("  ⚠ Failed to list resources from {name}: {e}");
                }
            }
        }
        result
    }

    /// Get a prompt from a specific server.
    pub async fn get_prompt(
        &self,
        server_name: &str,
        prompt_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<GetPromptResult, McpError> {
        let conn = self
            .connections
            .get(server_name)
            .ok_or_else(|| McpError::ServerNotConnected(server_name.to_string()))?;
        conn.get_prompt(prompt_name, arguments)
            .await
            .map_err(McpError::Service)
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

    /// Whether sampling (createMessage) support is configured.
    pub fn has_sampling(&self) -> bool {
        self.sampling.is_some()
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

        match connect_to_server(config, self.sampling.clone()).await {
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
        let connection = match connect_to_server(config, self.sampling.clone()).await {
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

    /// Check all connections for tool list change notifications and refresh
    /// any that have pending changes. Returns the names of servers that were refreshed.
    pub async fn refresh_changed_tools(&mut self) -> Vec<String> {
        let mut refreshed = Vec::new();
        for (name, conn) in &mut self.connections {
            if conn.has_pending_tool_change() {
                if let Some(inner) = Arc::get_mut(conn) {
                    match inner.refresh_tools_if_changed().await {
                        Ok(true) => refreshed.push(name.clone()),
                        Ok(false) => {}
                        Err(e) => eprintln!("  ⚠ Failed to refresh tools for {name}: {e}"),
                    }
                }
            }
        }
        if !refreshed.is_empty() {
            eprintln!("  ↻ Refreshed tool lists for: {}", refreshed.join(", "));
        }
        refreshed
    }

    /// Check all connections for prompt list change notifications and consume
    /// the flags. Returns the names of servers whose prompt lists changed.
    pub fn consume_prompt_changes(&self) -> Vec<String> {
        let mut changed = Vec::new();
        for (name, conn) in &self.connections {
            if conn.consume_prompt_change() {
                changed.push(name.clone());
            }
        }
        if !changed.is_empty() {
            eprintln!("  ↻ Prompt lists changed on: {}", changed.join(", "));
        }
        changed
    }

    /// Check all connections for resource change notifications and consume
    /// the flags. Returns the names of servers whose resources changed.
    pub fn consume_resource_changes(&self) -> Vec<String> {
        let mut changed = Vec::new();
        for (name, conn) in &self.connections {
            if conn.consume_resource_change() {
                changed.push(name.clone());
            }
        }
        if !changed.is_empty() {
            eprintln!("  ↻ Resources changed on: {}", changed.join(", "));
        }
        changed
    }
}

/// Thread-safe MCP client manager.
pub type SharedMcpClientManager = Arc<RwLock<McpClientManager>>;

/// Create a new shared MCP client manager.
pub fn new_shared_manager() -> SharedMcpClientManager {
    Arc::new(RwLock::new(McpClientManager::new()))
}

/// Connect to an MCP server with exponential backoff retry.
async fn connect_to_server(
    config: McpServerConfig,
    sampling: Option<Arc<SamplingConfig>>,
) -> Result<McpConnection, McpError> {
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

        match connect_once(&config, sampling.clone()).await {
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
async fn connect_once(
    config: &McpServerConfig,
    sampling: Option<Arc<SamplingConfig>>,
) -> Result<McpConnection, McpError> {
    match &config.transport {
        Transport::Stdio { command, args, env } => {
            connect_stdio(&config.name, command, args, env, config.clone(), sampling).await
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
                sampling,
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
                sampling,
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
    sampling: Option<Arc<SamplingConfig>>,
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

    // Connect as MCP client with change notification handler
    let (handler, tools_changed, prompts_changed, resources_changed) = ChangeHandler::new(sampling);
    let running = serve_client(handler, transport)
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
        tools_changed,
        prompts_changed,
        resources_changed,
    })
}

/// Connect via HTTP SSE (Streamable HTTP) transport.
async fn connect_sse(
    name: &str,
    url: &str,
    auth_token: Option<&str>,
    headers: &HashMap<String, String>,
    config: McpServerConfig,
    sampling: Option<Arc<SamplingConfig>>,
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

    // Connect as MCP client with change notification handler
    let (handler, tools_changed, prompts_changed, resources_changed) = ChangeHandler::new(sampling);
    let running = serve_client(handler, transport)
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
        tools_changed,
        prompts_changed,
        resources_changed,
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
    sampling: Option<Arc<SamplingConfig>>,
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

    // Connect as MCP client with change notification handler
    let (handler, tools_changed, prompts_changed, resources_changed) = ChangeHandler::new(sampling);
    let running = serve_client(handler, (rmcp_read, rmcp_write))
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
        tools_changed,
        prompts_changed,
        resources_changed,
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

    // ── ChangeHandler tests ───────────────────────────────────────────────

    #[test]
    fn change_handler_initial_state() {
        let (_handler, tools, prompts, resources) = ChangeHandler::new(None);
        assert!(!tools.load(Ordering::Acquire));
        assert!(!prompts.load(Ordering::Acquire));
        assert!(!resources.load(Ordering::Acquire));
    }

    #[test]
    fn change_handler_sets_tools_flag() {
        let (handler, tools, _prompts, _resources) = ChangeHandler::new(None);
        handler.tools_changed.store(true, Ordering::Release);
        assert!(tools.load(Ordering::Acquire));
    }

    #[test]
    fn change_handler_sets_prompts_flag() {
        let (handler, _tools, prompts, _resources) = ChangeHandler::new(None);
        handler.prompts_changed.store(true, Ordering::Release);
        assert!(prompts.load(Ordering::Acquire));
    }

    #[test]
    fn change_handler_sets_resources_flag() {
        let (handler, _tools, _prompts, resources) = ChangeHandler::new(None);
        handler.resources_changed.store(true, Ordering::Release);
        assert!(resources.load(Ordering::Acquire));
    }

    #[test]
    fn change_handler_flags_independent() {
        let (handler, tools, prompts, resources) = ChangeHandler::new(None);
        handler.tools_changed.store(true, Ordering::Release);
        assert!(tools.load(Ordering::Acquire));
        assert!(!prompts.load(Ordering::Acquire));
        assert!(!resources.load(Ordering::Acquire));

        handler.prompts_changed.store(true, Ordering::Release);
        assert!(prompts.load(Ordering::Acquire));
        assert!(!resources.load(Ordering::Acquire));

        handler.resources_changed.store(true, Ordering::Release);
        assert!(resources.load(Ordering::Acquire));
    }

    #[test]
    fn change_handler_clone_shares_flags() {
        let (handler, tools, prompts, resources) = ChangeHandler::new(None);
        let cloned = handler.clone();
        cloned.tools_changed.store(true, Ordering::Release);
        cloned.prompts_changed.store(true, Ordering::Release);
        cloned.resources_changed.store(true, Ordering::Release);
        assert!(tools.load(Ordering::Acquire));
        assert!(prompts.load(Ordering::Acquire));
        assert!(resources.load(Ordering::Acquire));
        assert!(handler.tools_changed.load(Ordering::Acquire));
        assert!(handler.prompts_changed.load(Ordering::Acquire));
        assert!(handler.resources_changed.load(Ordering::Acquire));
    }

    #[test]
    fn manager_refresh_changed_tools_empty() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut manager = McpClientManager::new();
            let refreshed = manager.refresh_changed_tools().await;
            assert!(refreshed.is_empty());
        });
    }

    #[test]
    fn manager_all_prompts_empty() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let manager = McpClientManager::new();
            let prompts = manager.all_prompts().await;
            assert!(prompts.is_empty());
        });
    }

    #[test]
    fn manager_get_prompt_no_server() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let manager = McpClientManager::new();
            let result = manager.get_prompt("nonexistent", "test", None).await;
            assert!(result.is_err());
            assert!(matches!(result.unwrap_err(), McpError::ServerNotConnected(_)));
        });
    }

    // ── Sampling tests ────────────────────────────────────────────────────

    #[test]
    fn sampling_messages_text_only() {
        let msgs = vec![
            SamplingMessage::user_text("Hello"),
            SamplingMessage::assistant_text("Hi there"),
        ];
        let openai = sampling_messages_to_openai(&msgs);
        assert_eq!(openai.len(), 2);
        assert_eq!(openai[0]["role"], "user");
        assert_eq!(openai[0]["content"], "Hello");
        assert_eq!(openai[1]["role"], "assistant");
        assert_eq!(openai[1]["content"], "Hi there");
    }

    #[test]
    fn sampling_messages_image_content() {
        use rmcp::model::RawImageContent;
        let img = SamplingMessageContent::Image(RawImageContent {
            mime_type: "image/png".into(),
            data: "abc123".into(),
            meta: None,
        });
        let msg = SamplingMessage::new(Role::User, img);
        let openai = sampling_messages_to_openai(&[msg]);
        assert_eq!(openai.len(), 1);
        let content = &openai[0]["content"];
        assert!(content.is_array());
        let part = &content[0];
        assert_eq!(part["type"], "image_url");
        assert_eq!(
            part["image_url"]["url"],
            "data:image/png;base64,abc123"
        );
    }

    #[test]
    fn sampling_messages_empty() {
        let openai = sampling_messages_to_openai(&[]);
        assert!(openai.is_empty());
    }

    #[test]
    fn sampling_config_debug_hides_token() {
        let config = SamplingConfig {
            api: Arc::new(
                astra_thin_client::ThinClient::new("http://localhost:8000", None).unwrap(),
            ),
            token: "secret-token-123".to_string(),
            model: "test-model".to_string(),
        };
        let debug = format!("{config:?}");
        assert!(debug.contains("test-model"));
        assert!(!debug.contains("secret-token-123"));
    }

    #[test]
    fn change_handler_with_sampling_config() {
        let config = SamplingConfig {
            api: Arc::new(
                astra_thin_client::ThinClient::new("http://localhost:8000", None).unwrap(),
            ),
            token: "tok".to_string(),
            model: "m".to_string(),
        };
        let (handler, _t, _p, _r) = ChangeHandler::new(Some(Arc::new(config)));
        assert!(handler.sampling.is_some());
    }

    #[test]
    fn change_handler_without_sampling() {
        let (handler, _t, _p, _r) = ChangeHandler::new(None);
        assert!(handler.sampling.is_none());
    }

    #[test]
    fn manager_set_sampling_config() {
        let mut manager = McpClientManager::new();
        assert!(manager.sampling.is_none());

        let config = SamplingConfig {
            api: Arc::new(
                astra_thin_client::ThinClient::new("http://localhost:8000", None).unwrap(),
            ),
            token: "tok".to_string(),
            model: "m".to_string(),
        };
        manager.set_sampling_config(Some(config));
        assert!(manager.sampling.is_some());

        manager.set_sampling_config(None);
        assert!(manager.sampling.is_none());
    }
}
