//! Mock MCP server for integration testing.
//!
//! Exposes three tools: echo, add, get_time.
//! Uses stdio transport — spawn as a subprocess from integration tests.
//!
//! Prefer the bin target for tests (`target/debug/mock_mcp_server`); this
//! example remains for manual ad-hoc runs.

#[path = "../src/mock_mcp_server_impl.rs"]
mod mock_mcp_server_impl;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    mock_mcp_server_impl::run_mock_mcp_server().await
}
