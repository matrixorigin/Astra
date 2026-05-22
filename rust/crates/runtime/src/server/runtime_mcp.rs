//! Runtime MCP manager — server-side MCP connection lifecycle.
//!
//! Parses `context.mcp_servers` from the chat request, connects to MCP
//! servers, discovers tools, and provides schema injection + tool dispatch
//! for the server-side agent loop.

use std::collections::HashMap;
use std::sync::Arc;

use astra_mcp::{McpClientManager, McpServerConfig, Transport};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::RwLock;

use astra_core::ErrorResponse;

/// A single MCP server entry from `context.mcp_servers`.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct ContextMcpServer {
    pub name: String,
    #[serde(alias = "type")]
    pub transport: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub forward_headers: Vec<String>,
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
}

/// Parsed MCP server configuration with header filtering applied.
#[derive(Debug, Clone)]
struct ResolvedMcpConfig {
    mcp_config: McpServerConfig,
    allowed_tools: Option<Vec<String>>,
}

/// Runtime MCP manager — connects to MCP servers, provides schemas and tool dispatch.
pub(crate) struct RuntimeMcpManager {
    inner: Arc<RwLock<McpClientManager>>,
}

impl RuntimeMcpManager {
    /// Parse `context.mcp_servers` from the chat request, connect to all servers
    /// with header forwarding, and return the manager with discovered tools.
    pub async fn connect_all(
        context_servers: &[ContextMcpServer],
        forward_headers: &HashMap<String, String>,
    ) -> Result<Self, ErrorResponse> {
        let mut manager = McpClientManager::new();

        for server in context_servers {
            let resolved = Self::resolve_config(server, forward_headers)?;
            manager
                .connect(resolved.mcp_config)
                .await
                .map_err(|e| {
                    ErrorResponse::new(format!(
                        "MCP discovery failed for server '{}': {e}",
                        server.name
                    ))
                    .with_error_code("mcp_discovery_failed")
                })?;

            if let Some(ref allowed) = resolved.allowed_tools {
                tracing::info!(
                    "MCP server '{}': {} tools discovered, {} allowed",
                    server.name,
                    manager
                        .get(&server.name)
                        .map(|c| c.tools().len())
                        .unwrap_or(0),
                    allowed.len()
                );
            }
        }

        Ok(Self {
            inner: Arc::new(RwLock::new(manager)),
        })
    }

    /// Resolve a single MCP server config: validate, apply header forwarding,
    /// and convert to `McpServerConfig`.
    fn resolve_config(
        server: &ContextMcpServer,
        forward_headers: &HashMap<String, String>,
    ) -> Result<ResolvedMcpConfig, ErrorResponse> {
        let transport = match server.transport.as_str() {
            "sse" | "http" => {
                let url = server.url.as_deref().ok_or_else(|| {
                    ErrorResponse::new(format!(
                        "MCP server '{}': SSE transport requires `url`",
                        server.name
                    ))
                    .with_error_code("mcp_discovery_failed")
                })?;

                // Filter headers: only forward those declared in server.forward_headers
                let mut headers = HashMap::new();
                for header_name in &server.forward_headers {
                    let lower = header_name.to_ascii_lowercase();
                    match forward_headers.get(&lower) {
                        Some(value) => {
                            headers.insert(header_name.clone(), value.clone());
                        }
                        None => {
                            return Err(ErrorResponse::new(format!(
                                "MCP server '{}': required header '{}' not found in forwarded headers",
                                server.name, header_name
                            ))
                            .with_error_code("mcp_discovery_failed"));
                        }
                    }
                }

                Transport::Sse {
                    url: url.to_string(),
                    auth_token: None,
                    headers,
                }
            }
            "stdio" => {
                let command = server.command.as_deref().ok_or_else(|| {
                    ErrorResponse::new(format!(
                        "MCP server '{}': stdio transport requires `command`",
                        server.name
                    ))
                    .with_error_code("mcp_discovery_failed")
                })?;
                Transport::Stdio {
                    command: vec![command.to_string()],
                    args: server.args.clone(),
                    env: HashMap::new(),
                }
            }
            other => {
                return Err(ErrorResponse::new(format!(
                    "MCP server '{}': unsupported transport '{}' for runtime MCP",
                    server.name, other
                ))
                .with_error_code("mcp_discovery_failed"));
            }
        };

        let mcp_config = McpServerConfig {
            name: server.name.clone(),
            transport,
            description: String::new(),
            enabled: true,
            retry: Default::default(),
        };

        Ok(ResolvedMcpConfig {
            mcp_config,
            allowed_tools: server.allowed_tools.clone(),
        })
    }

    /// Get all MCP tool schemas for injection into the LLM tool surface.
    pub async fn tool_schemas(&self) -> Vec<Value> {
        self.inner.read().await.all_tool_schemas()
    }

    /// Get the inner manager reference for MCP tool execution.
    pub fn inner(&self) -> &Arc<RwLock<McpClientManager>> {
        &self.inner
    }
}
