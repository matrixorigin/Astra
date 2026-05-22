//! Shared MCP (Model Context Protocol) client library.
//!
//! Provides transport-agnostic MCP server connection, tool discovery,
//! schema conversion, and tool dispatch. Used by both the CLI (edge agent)
//! and the server runtime.

mod connection;
mod error;
mod manager;
mod tools;
mod types;

pub use connection::McpConnection;
pub use error::McpError;
pub use manager::McpClientManager;
pub use tools::{
    extract_result_text, extract_result_text_with_limit, is_dangerous_env_var,
    mcp_tool_to_schema, sanitize_tool_name,
    MAX_DESCRIPTION_LENGTH, MAX_RESULT_CONTENT_LENGTH,
};
pub use types::{ConnectionState, McpServerConfig, RetryConfig, Transport};

/// Timeout for MCP server connection (seconds).
pub const MCP_CONNECT_TIMEOUT_SECS: u64 = 30;

/// Timeout for MCP tool calls (seconds).
pub const MCP_TOOL_CALL_TIMEOUT_SECS: u64 = 120;
