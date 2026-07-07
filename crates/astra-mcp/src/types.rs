use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// MCP server transport configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Transport {
    /// Stdio transport: communicates via stdin/stdout with a child process.
    Stdio {
        command: Vec<String>,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    /// Classic HTTP SSE transport: GET opens an SSE stream, POST sends JSON-RPC to the endpoint event.
    #[serde(alias = "sse")]
    Sse {
        url: String,
        #[serde(default)]
        auth_token: Option<String>,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    /// Streamable HTTP transport (MCP 2025-03-26 spec).
    #[serde(rename = "streamable_http", alias = "http", alias = "streamable-http")]
    StreamableHttp {
        url: String,
        #[serde(default)]
        auth_token: Option<String>,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    /// WebSocket transport.
    #[serde(alias = "websocket")]
    Ws {
        url: String,
        #[serde(default)]
        auth_token: Option<String>,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

/// Connection state for an MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
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
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_initial_delay_ms")]
    pub initial_delay_ms: u64,
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
    pub name: String,
    pub transport: Transport,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub retry: RetryConfig,
}

fn default_true() -> bool {
    true
}
