use rmcp::ServiceError;

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
