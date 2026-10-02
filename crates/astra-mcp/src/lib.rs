//! Shared MCP (Model Context Protocol) client library.
//!
//! Provides transport-agnostic MCP server connection, tool discovery,
//! schema conversion, and tool dispatch. Used by both the CLI (edge agent)
//! and the server runtime.

mod classic_sse;
mod connection;
mod error;
mod manager;
mod tools;
mod types;

pub use connection::{CallLogEntry, McpConnection};
pub use error::McpError;
pub use manager::{
    McpClientManager, McpToolCollision, McpToolCollisionSource, PreparedMcpConnection,
    PreparedMcpToolCall,
};
pub use rmcp::model::Tool as McpTool;
pub use tools::{
    MAX_DESCRIPTION_LENGTH, MAX_RESULT_CONTENT_LENGTH, McpToolCallResult,
    WORKSPACE_EFFECT_METADATA_KEY, WORKSPACE_EFFECT_SETTLED_FIELD, extract_result_text,
    extract_result_text_with_limit, extract_tool_call_result_with_limit, is_dangerous_env_var,
    mcp_provider_snapshot_to_schemas_checked, mcp_resolved_provider_snapshot_to_schemas_checked,
    mcp_tool_schema_from_parts, mcp_tool_to_provider_declaration, mcp_tool_to_schema,
    mcp_tools_to_provider_snapshot, sanitize_tool_name, tools_to_schemas_checked,
    workspace_effect_is_settled,
};
pub use types::{ConnectionState, McpServerConfig, RetryConfig, Transport};

/// Timeout for MCP server connection (seconds).
pub const MCP_CONNECT_TIMEOUT_SECS: u64 = 30;

/// Timeout for MCP tool calls (seconds).
pub const MCP_TOOL_CALL_TIMEOUT_SECS: u64 = 120;
